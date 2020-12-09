/*
 * Copyright (C) 2017 Open Whisper Systems
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 */
package org.whispersystems.contactdiscovery.resources;

import com.codahale.metrics.annotation.ResponseMetered;
import com.codahale.metrics.annotation.Timed;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.google.common.io.CharStreams;
import io.dropwizard.auth.Auth;
import org.bouncycastle.crypto.CryptoException;
import org.bouncycastle.crypto.digests.SHA256Digest;
import org.bouncycastle.crypto.params.AsymmetricKeyParameter;
import org.bouncycastle.crypto.signers.RSADigestSigner;
import org.bouncycastle.crypto.util.PrivateKeyFactory;
import org.bouncycastle.openssl.PEMKeyPair;
import org.bouncycastle.openssl.PEMParser;
import org.bouncycastle.util.encoders.Base64;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.whispersystems.contactdiscovery.auth.User;
import org.whispersystems.contactdiscovery.enclave.NoSuchEnclaveException;
import org.whispersystems.contactdiscovery.enclave.SgxException;
import org.whispersystems.contactdiscovery.enclave.SgxHandshakeManager;
import org.whispersystems.contactdiscovery.enclave.SignedQuoteUnavailableException;
import org.whispersystems.contactdiscovery.entities.MultipleRemoteAttestationResponse;
import org.whispersystems.contactdiscovery.entities.RemoteAttestationRequest;
import org.whispersystems.contactdiscovery.entities.RemoteAttestationResponse;
import org.whispersystems.contactdiscovery.limits.RateLimitExceededException;
import org.whispersystems.contactdiscovery.limits.RateLimiter;
import org.whispersystems.contactdiscovery.phonelimiter.PhoneRateLimiter;
import org.whispersystems.contactdiscovery.requests.RequestManager;

import javax.validation.Valid;
import javax.ws.rs.Consumes;
import javax.ws.rs.HeaderParam;
import javax.ws.rs.PUT;
import javax.ws.rs.Path;
import javax.ws.rs.PathParam;
import javax.ws.rs.Produces;
import javax.ws.rs.WebApplicationException;
import javax.ws.rs.core.HttpHeaders;
import javax.ws.rs.core.MediaType;
import javax.ws.rs.core.Response;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.Reader;
import java.io.StringReader;
import java.io.UnsupportedEncodingException;
import java.net.URLEncoder;
import java.util.HashMap;
import java.util.Map;
import java.util.function.Function;

/**
 * API endpoint for doing remote attestation of and simultaneously establishing a secure
 * connection to an SGX enclave
 *
 * @author Moxie Marlinspike
 */

@Path("/v1/attestation/")
@ResponseMetered
public class RemoteAttestationResource {

  private final Logger logger = LoggerFactory.getLogger(RemoteAttestationResource.class);

  private final SgxHandshakeManager sgxHandshakeManager;
  private final RateLimiter rateLimiter;
  private final PhoneRateLimiter client;

  public RemoteAttestationResource(SgxHandshakeManager sgxHandshakeManager, RateLimiter rateLimiter, PhoneRateLimiter client) {
    this.sgxHandshakeManager = sgxHandshakeManager;
    this.rateLimiter = rateLimiter;
    this.client = client;
  }

  @Timed
  @PUT
  @Path("/{enclaveId}")
  @Consumes(MediaType.APPLICATION_JSON)
  @Produces(MediaType.APPLICATION_JSON)
  @RequestLimiter
  public Response getAttestationHandshake(@Auth User user,
                                          @PathParam("enclaveId") String enclaveId,
                                          @HeaderParam(HttpHeaders.AUTHORIZATION) String authHeader,
                                          @Valid RemoteAttestationRequest request)
      throws NoSuchEnclaveException, SignedQuoteUnavailableException, SgxException, RateLimitExceededException
  {
    rateLimiter.validate(user.getNumber());
    var svcFuture = client.attest(user, authHeader, enclaveId, request.getClientPublic());
    RemoteAttestationResponse attestation = sgxHandshakeManager.getHandshake(enclaveId, request.getClientPublic());

    // The exceptionally here is because we want to test the performance of the new rate limit service but not rely on
    // it to respond, yet. See CDS-157.
    return svcFuture.exceptionally((ex) -> new HashMap<>()).thenApply((resps) -> {
      resps.put(RequestManager.LOCAL_ENCLAVE_HOST_ID, attestation);
      return Response.ok(new MultipleRemoteAttestationResponse(resps)).build();
    }).join();
  }

  @PUT
  @Path("/test/{testName}/{enclaveId}")
  @Consumes(MediaType.APPLICATION_JSON)
  @Produces(MediaType.APPLICATION_JSON)
  public MultipleRemoteAttestationResponse getAttestationHandshake(@Auth User user,
                                                                   @PathParam("testName") String testName,
                                                                   @HeaderParam(HttpHeaders.AUTHORIZATION) String authHeader,
                                                                   @PathParam("enclaveId") String enclaveId,
                                                                   @Valid RemoteAttestationRequest request)
      throws NoSuchEnclaveException, SignedQuoteUnavailableException, SgxException, RateLimitExceededException
  {
    rateLimiter.validate(user.getNumber());
    Function<RemoteAttestationResponse, RemoteAttestationResponse> testFun;
    if ("bad-tag".equals(testName)) {
      testFun = response -> {
        response.getTag()[0] ^= 0xFF;
        return response;
      };
    } else if ("bad-quote".equals(testName)) {
      testFun = response -> {
        response.getQuote()[16] ^= 0xFF;
        return response;
      };
    } else if ("bad-server-static-public".equals(testName)) {
      testFun = response -> {
        response.getServerStaticPublic()[0] ^= 0xFF;
        return response;
      };
    } else if ("bad-signature".equals(testName)) {
      testFun = response -> {
        byte[] signature = Base64.decode(response.getSignature());
        signature[0] ^= 0xFF;
        return new RemoteAttestationResponse(response.getServerEphemeralPublic(),
                                             response.getServerStaticPublic(),
                                             response.getIv(),
                                             response.getCiphertext(),
                                             response.getTag(),
                                             response.getQuote(),
                                             new String(Base64.encode(signature)),
                                             response.getCertificates(),
                                             response.getSignatureBody());
      };
    } else if ("expired".equals(testName)) {
      return readMockRemoteAttestationResponse();
    } else if ("no-certificates".equals(testName)) {
      testFun = response ->
          new RemoteAttestationResponse(response.getServerEphemeralPublic(),
                                        response.getServerStaticPublic(),
                                        response.getIv(),
                                        response.getCiphertext(),
                                        response.getTag(),
                                        response.getQuote(),
                                        response.getSignature(),
                                        "",
                                        response.getSignatureBody());
    } else if ("wrong-certificates".equals(testName)) {
      testFun = response ->
          new RemoteAttestationResponse(response.getServerEphemeralPublic(),
                                        response.getServerStaticPublic(),
                                        response.getIv(),
                                        response.getCiphertext(),
                                        response.getTag(),
                                        response.getQuote(),
                                        response.getSignature(),
                                        readMockCertificate(),
                                        response.getSignatureBody());
    } else if ("untrusted-certificates".equals(testName)) {
      testFun = response -> {
        String mockSignature;
        try {
          byte[] signatureBodyBytes = response.getSignatureBody().getBytes("UTF-8");

          Reader mockKeyPairReader = new StringReader(readResourceAsString("/test/mock_private_key.pem"));
          PEMKeyPair mockKeyPair = (PEMKeyPair) new PEMParser(mockKeyPairReader).readObject();

          AsymmetricKeyParameter mockPrivateKey = PrivateKeyFactory.createKey(mockKeyPair.getPrivateKeyInfo());
          RSADigestSigner mockSigner = new RSADigestSigner(new SHA256Digest());

          mockSigner.init(true, mockPrivateKey);
          mockSigner.update(signatureBodyBytes, 0, signatureBodyBytes.length);
          mockSignature = new String(Base64.encode(mockSigner.generateSignature()));
        } catch (CryptoException | IOException ex) {
          throw new AssertionError(ex);
        }
        return new RemoteAttestationResponse(response.getServerEphemeralPublic(),
                                             response.getServerStaticPublic(),
                                             response.getIv(),
                                             response.getCiphertext(),
                                             response.getTag(),
                                             response.getQuote(),
                                             mockSignature,
                                             readMockCertificate(),
                                             response.getSignatureBody());
      };
    } else {
      throw new WebApplicationException(404);
    }

    RemoteAttestationResponse attestation = testFun.apply(sgxHandshakeManager.getHandshake(enclaveId, request.getClientPublic()));
    return new MultipleRemoteAttestationResponse(Map.of("enclave", attestation));
  }

  private static String readMockCertificate() {
    try {
      return URLEncoder.encode(readResourceAsString("/test/mock_certificate.pem"), "UTF-8");
    } catch (UnsupportedEncodingException ex) {
      throw new AssertionError(ex);
    }
  }

  private static MultipleRemoteAttestationResponse readMockRemoteAttestationResponse() {
    try {
      return new ObjectMapper().readValue(readResourceAsString("/test/mock_attestation_response.json"),
                                          MultipleRemoteAttestationResponse.class);
    } catch (IOException ex) {
      throw new AssertionError(ex);
    }
  }

  private static String readResourceAsString(String name) {
    try {
      return CharStreams.toString(new InputStreamReader(RemoteAttestationResource.class.getResourceAsStream(name)));
    } catch (IOException ex) {
      throw new AssertionError(ex);
    }
  }
}
