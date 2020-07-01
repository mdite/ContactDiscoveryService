//
// Copyright (C) 2019, 2020 Signal Messenger, LLC.
// All rights reserved.
//
// SPDX-License-Identifier: AGPL-3.0-or-later
//

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::TryInto;
use core::ffi::c_void;
use core::hash::{BuildHasher, Hash, Hasher};
use core::iter;
use core::mem;
use core::num::{NonZeroU128, NonZeroU32};
use core::ops::{Deref, DerefMut};
use core::slice;

use hashbrown::*;
use rand_core::RngCore;
use sgx_ffi::sgx::*;
use sgx_ffi::untrusted_slice::UntrustedSlice;
use sgx_ffi::util::{memset_s, SecretValue, ToUsize};
use sgxsd_ffi::ecalls::*;
use sgxsd_ffi::{AesGcmIv, AesGcmKey, AesGcmMac, RdRand, SHA1Context, SHA256Context};
use spin::*;
use subtle::{Choice, ConditionallySelectable, ConstantTimeEq};

use crate::ffi::cttk::*;
use crate::ffi::hash_lookup::*;
use crate::ffi::ratelimit_set::*;
use crate::ffi::sgxsd::*;
use crate::hasher::DefaultHasher;

//
// public API
//

pub struct SgxsdServerState {
    requests:            Vec<PendingRequest>,
    query_phones:        PhoneList,
    ratelimit_state_map: Option<Arc<RatelimitStateMap>>,
}

//
// internal
//

const BYTES_PER_PHONE: usize = mem::size_of::<Phone>();
const BYTES_PER_UUID: usize = mem::size_of::<Uuid>();

const COMMITMENT_NONCE_SIZE: usize = 32;

struct PhoneList(Vec<Phone>);

struct RatelimitStateMap {
    hash_map: RwLock<HashMap<NonZeroU128, Arc<Mutex<Option<RatelimitState>>>, DefaultHasher>>,
    hasher:   DefaultHasher,
}

struct RatelimitState {
    nonce: NonZeroU32,
    key:   AesGcmKey,
}

struct RatelimitStateData {
    data: SecretValue<Box<[u8]>>,
}

struct PendingRequest {
    from:                SgxsdMsgFrom,
    request_phone_count: u32,
}

struct Request<'a> {
    phones:          RequestPhoneList,
    ratelimit_state: Option<RequestRatelimitState<'a>>,
}

struct RequestPhoneList {
    data: SecretValue<Box<[u8]>>,
}

struct RequestRatelimitState<'a> {
    uuid: NonZeroU128,
    data: UntrustedSlice<'a>,
}

//
// public free functions
//

pub fn update_ratelimit_state(
    ratelimit_state_uuid: Uuid,
    encrypted_ratelimit_state: &mut [u8],
    query_phones: &[Phone],
) -> Result<(), SgxStatus>
{
    let ratelimit_state_uuid: Option<NonZeroU128> = ratelimit_state_uuid.into();
    let ratelimit_state_uuid = ratelimit_state_uuid.ok_or(SGX_ERROR_INVALID_PARAMETER)?;

    let (ratelimit_state_data, ratelimit_state_mac_data) =
        match encrypted_ratelimit_state.len().checked_sub(AesGcmMac::default().data.len()) {
            Some(ratelimit_state_data_len) => encrypted_ratelimit_state.split_at_mut(ratelimit_state_data_len),
            None => return Err(CDS_ERROR_INVALID_RATE_LIMIT_STATE),
        };
    let ratelimit_state_mac_data: &mut _ = ratelimit_state_mac_data.try_into().unwrap_or_else(|_| unreachable!());

    let ratelimit_state_lock = RatelimitStateMap::global(0).get(&ratelimit_state_uuid);
    let mut locked_ratelimit_state = ratelimit_state_lock.lock();

    let ratelimit_state: &mut RatelimitState = locked_ratelimit_state.get_or_insert_with(Default::default);

    let (new_ratelimit_state_data, new_ratelimit_state_mac) = ratelimit_state.update(
        SecretValue::new(ratelimit_state_data.to_vec().into_boxed_slice()),
        AesGcmMac {
            data: *ratelimit_state_mac_data,
        },
        &query_phones,
    )?;

    ratelimit_state_data.copy_from_slice(&new_ratelimit_state_data);
    *ratelimit_state_mac_data = new_ratelimit_state_mac.data;

    Ok(())
}

pub fn delete_ratelimit_state(ratelimit_state_uuid: Uuid) -> Result<(), SgxStatus> {
    let ratelimit_state_uuid: Option<_> = ratelimit_state_uuid.into();
    let ratelimit_state_uuid = ratelimit_state_uuid.ok_or(SGX_ERROR_INVALID_PARAMETER)?;
    let ratelimit_state_lock = RatelimitStateMap::global(0).get(&ratelimit_state_uuid);
    *ratelimit_state_lock.lock() = None;
    Ok(())
}

//
// SgxsdServerState
//

impl SgxsdServerState {
    fn decode_request<'a>(&mut self, args: &'a CallArgs, request_data: &[u8]) -> Result<Request<'a>, SgxStatus> {
        if (args.query_phone_count == 0 || args.query_phone_count.to_usize() > self.query_phones.capacity() - self.query_phones.len()) {
            return Err(SGX_ERROR_INVALID_PARAMETER);
        }

        let query_data_slice = UntrustedSlice::new(args.query.data, args.query.size.to_usize()).map_err(|_| SGX_ERROR_INVALID_PARAMETER)?;
        let mut query_phones = RequestPhoneList::new(
            query_data_slice
                .read_bytes(args.query.size.to_usize())
                .map_err(|_| SGX_ERROR_INVALID_PARAMETER)?
                .into_boxed_slice(),
        );
        let query_phones_data_len = (query_phones.data.get().len())
            .checked_sub(COMMITMENT_NONCE_SIZE)
            .ok_or(CDS_ERROR_INVALID_REQUEST_SIZE)?;

        if (request_data.len() != AesGcmKey::len() ||
            query_phones_data_len % BYTES_PER_PHONE != 0 ||
            query_phones_data_len / BYTES_PER_PHONE != args.query_phone_count.to_usize())
        {
            return Err(CDS_ERROR_INVALID_REQUEST_SIZE);
        }

        let query_key = AesGcmKey::new(request_data)?;
        query_key.decrypt(&mut query_phones.data.get_mut()[..], &[], &args.query.iv, &args.query.mac)?;

        Self::verify_commitment(&query_phones.data.get()[..], &args.query_commitment)?;

        let ratelimit_state = if let Some(ratelimit_state_uuid) = args.ratelimit_state_uuid.into() {
            Some(RequestRatelimitState {
                uuid: ratelimit_state_uuid,
                data: UntrustedSlice::new(args.ratelimit_state_data, args.ratelimit_state_size.to_usize())
                    .map_err(|_| SGX_ERROR_INVALID_PARAMETER)?,
            })
        } else {
            None
        };
        Ok(Request {
            phones: query_phones,
            ratelimit_state,
        })
    }

    fn verify_commitment(data: &[u8], expected_commitment: &[u8; SHA256Context::hash_len()]) -> Result<(), SgxStatus> {
        let mut context: SHA256Context = Default::default();
        context.update(data);

        let mut commitment: [u8; SHA256Context::hash_len()] = Default::default();
        context.result(&mut commitment);

        if &commitment == expected_commitment {
            Ok(())
        } else {
            Err(CDS_ERROR_QUERY_COMMITMENT_MISMATCH)
        }
    }

    fn update_ratelimit_state(
        &mut self,
        mut query_phones: PhoneList,
        request_ratelimit_state: RequestRatelimitState<'_>,
    ) -> Result<(), SgxStatus>
    {
        let ratelimit_state_data_len = (request_ratelimit_state.data)
            .len()
            .checked_sub(AesGcmMac::default().data.len())
            .ok_or(CDS_ERROR_INVALID_RATE_LIMIT_STATE)?;
        let ratelimit_state_data = SecretValue::new(
            (request_ratelimit_state.data)
                .read_bytes(ratelimit_state_data_len)
                .map_err(|_| CDS_ERROR_INVALID_RATE_LIMIT_STATE)?
                .into_boxed_slice(),
        );

        let ratelimit_state_mac = AesGcmMac {
            data: (request_ratelimit_state.data)
                .offset(ratelimit_state_data_len)
                .read_bytes(AesGcmMac::default().data.len())
                .map_err(|_| CDS_ERROR_INVALID_RATE_LIMIT_STATE)?[..]
                .try_into()
                .unwrap_or_else(|_| static_unreachable!()),
        };

        let ratelimit_state_lock = (self.ratelimit_state_map.as_mut())
            .ok_or(SGX_ERROR_INVALID_STATE)?
            .get(&request_ratelimit_state.uuid);
        let mut locked_ratelimit_state = ratelimit_state_lock.lock();

        let ratelimit_state: &mut RatelimitState = locked_ratelimit_state.get_or_insert_with(Default::default);

        for query_phone in &mut query_phones[..] {
            hash_query_phone(query_phone);
        }

        let (ratelimit_state_data, ratelimit_state_mac) =
            ratelimit_state.update(ratelimit_state_data, ratelimit_state_mac, &query_phones)?;

        (request_ratelimit_state.data)
            .write_bytes(&ratelimit_state_data)
            .map_err(|()| SGX_ERROR_UNEXPECTED)?;
        (request_ratelimit_state.data)
            .offset(ratelimit_state_data_len)
            .write_bytes(&ratelimit_state_mac.data)
            .map_err(|()| SGX_ERROR_UNEXPECTED)?;

        Ok(())
    }
}

impl SgxsdServer for SgxsdServerState {
    type HandleCallArgs = CallArgs;
    type InitArgs = StartArgs;
    type TerminateArgs = StopArgs;

    fn init(args: Option<&StartArgs>) -> Result<Self, SgxStatus> {
        let args = args.ok_or(SGX_ERROR_INVALID_PARAMETER)?;

        let ratelimit_state_map = match args.max_ratelimit_states {
            0 => None,
            _ => Some(RatelimitStateMap::global(args.max_ratelimit_states.to_usize())),
        };

        Ok(Self {
            requests: Vec::with_capacity(args.max_query_phones.to_usize() / 4),
            query_phones: PhoneList::new(args.max_query_phones.to_usize()),
            ratelimit_state_map,
        })
    }

    fn handle_call(&mut self, args: Option<&CallArgs>, request_data: &[u8], from: SgxsdMsgFrom) -> Result<(), (SgxStatus, SgxsdMsgFrom)> {
        let args = match args {
            Some(args) => args,
            None => return Err((SGX_ERROR_INVALID_PARAMETER, from)),
        };
        let request = match self.decode_request(args, request_data) {
            Ok(request) => request,
            Err(error) => return Err((error, from)),
        };

        if let Some(ratelimit_state) = request.ratelimit_state {
            let request_phones_iter = request.phones.iter();
            let mut request_phones = PhoneList::new(request_phones_iter.len());
            request_phones.extend(request_phones_iter);
            self.update_ratelimit_state(request_phones, ratelimit_state)
                .map_err(|error: SgxStatus| (error, from))
        } else {
            let request_phones_iter = request.phones.iter();
            let request_phone_count = match request_phones_iter.len().try_into() {
                Ok(request_phone_count) => request_phone_count,
                Err(_) => return Err((SGX_ERROR_INVALID_PARAMETER, from)),
            };
            self.query_phones.extend(request_phones_iter);
            self.requests.push(PendingRequest { from, request_phone_count });
            Ok(())
        }
    }

    fn terminate(self, args: Option<&StopArgs>) -> Result<(), SgxStatus> {
        let args = args.ok_or(SGX_ERROR_INVALID_PARAMETER)?;

        let in_phones_size = (args.in_phone_count)
            .checked_mul(BYTES_PER_PHONE)
            .ok_or(SGX_ERROR_INVALID_PARAMETER)?;
        let in_uuids_size = (args.in_phone_count)
            .checked_mul(BYTES_PER_UUID)
            .ok_or(SGX_ERROR_INVALID_PARAMETER)?;

        let in_phones = UntrustedSlice::new(args.in_phones as *mut u8, in_phones_size).map_err(|_| SGX_ERROR_INVALID_PARAMETER)?;
        let in_uuids = UntrustedSlice::new(args.in_uuids as *mut u8, in_uuids_size).map_err(|_| SGX_ERROR_INVALID_PARAMETER)?;

        let query_phones_chunks = self.query_phones.chunks(MAX_HASH_TABLE_SIZE);
        let in_query_phones_result_len = (self.query_phones)
            .len()
            .checked_mul(BYTES_PER_UUID)
            .ok_or(SGX_ERROR_INVALID_PARAMETER)?;
        let mut in_query_phones_result = SecretValue::new(vec![0u8; in_query_phones_result_len]);
        let in_query_phones_result_chunks = (in_query_phones_result.get_mut()).chunks_mut(MAX_HASH_TABLE_SIZE * BYTES_PER_UUID);
        for (query_phones_chunk, in_query_phones_result_chunk) in query_phones_chunks.zip(in_query_phones_result_chunks) {
            unsafe {
                hash_lookup(
                    in_phones.as_ptr(),
                    in_uuids.as_ptr(),
                    args.in_phone_count,
                    query_phones_chunk,
                    in_query_phones_result_chunk,
                )?;
            }
        }

        let mut in_query_phones_result_remaining = &mut in_query_phones_result.get_mut()[..];
        for request in self.requests {
            let (request_in_query_phones_result, in_query_phones_result_rest) =
                in_query_phones_result_remaining.split_at_mut(request.request_phone_count.to_usize() * BYTES_PER_UUID);
            request.from.reply(request_in_query_phones_result)?;
            in_query_phones_result_remaining = in_query_phones_result_rest;
        }

        Ok(())
    }
}

//
// PhoneList
//

impl PhoneList {
    pub fn new(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }
}

impl Drop for PhoneList {
    fn drop(&mut self) {
        let byte_len = self.0.len() * mem::size_of::<Phone>();
        let clear_res = unsafe { memset_s(self.0.as_mut_ptr() as *mut c_void, byte_len, 0, byte_len) };
        assert_eq!(clear_res, 0);
    }
}

impl Deref for PhoneList {
    type Target = Vec<Phone>;

    fn deref(&self) -> &Vec<Phone> {
        &self.0
    }
}
impl DerefMut for PhoneList {
    fn deref_mut(&mut self) -> &mut Vec<Phone> {
        &mut self.0
    }
}

//
// RatelimitStateMap
//

static RATELIMIT_STATE_MAP: spin::Once<Arc<RatelimitStateMap>> = Once::new();

impl RatelimitStateMap {
    fn global(capacity: usize) -> Arc<Self> {
        Arc::clone(RATELIMIT_STATE_MAP.call_once(|| Arc::new(Self::new(capacity))))
    }

    fn new(capacity: usize) -> Self {
        let hasher = DefaultHasher::default();
        Self {
            hash_map: RwLock::new(HashMap::with_capacity_and_hasher(capacity, hasher.clone())),
            hasher,
        }
    }

    fn get(&self, key: &NonZeroU128) -> Arc<Mutex<Option<RatelimitState>>> {
        let mut hasher = self.hasher.build_hasher();
        key.hash(&mut hasher);
        let hash = hasher.finish();

        let readable_map = self.hash_map.upgradeable_read();
        if let Some((_, ratelimit_state)) = readable_map.raw_entry().from_key_hashed_nocheck(hash, key) {
            Arc::clone(ratelimit_state)
        } else {
            let mut writable_map = readable_map.upgrade();
            let (_, ratelimit_state) = writable_map
                .raw_entry_mut()
                .from_key_hashed_nocheck(hash, key)
                .or_insert_with(|| (*key, Default::default()));
            Arc::clone(ratelimit_state)
        }
    }
}

//
// RatelimitState
//

impl RatelimitState {
    fn get_iv(&self) -> AesGcmIv {
        let mut iv: AesGcmIv = Default::default();
        let nonce_bytes = self.nonce.get().to_le_bytes();
        iv.data[..nonce_bytes.len()].copy_from_slice(&nonce_bytes);
        iv
    }

    pub fn update(
        &mut self,
        mut ratelimit_state_data: SecretValue<Box<[u8]>>,
        mut ratelimit_state_mac: AesGcmMac,
        query_phones: &[u64],
    ) -> Result<(Box<[u8]>, AesGcmMac), SgxStatus>
    {
        let ratelimit_state_data = if !ratelimit_state_data.get().iter().all(|b: &u8| b == &0) {
            self.key
                .decrypt(ratelimit_state_data.get_mut(), &[], &self.get_iv(), &ratelimit_state_mac)
                .map_err(|_| CDS_ERROR_INVALID_RATE_LIMIT_STATE)?;
            RatelimitStateData::new(ratelimit_state_data)
        } else {
            ratelimit_state_data.clear();
            let mut ratelimit_state_data = RatelimitStateData::new(ratelimit_state_data);
            let ratelimit_state_slot_count = ratelimit_state_data.slot_count();
            ratelimit_state_data.set_size_limit(ratelimit_state_slot_count / 2, ratelimit_state_slot_count / 2)?;
            ratelimit_state_data
        };

        // increment nonce before revealing the result, to prevent replay of information leakage
        let encrypt_nonce = (self.nonce.get()).checked_add(1).ok_or(CDS_ERROR_INVALID_RATE_LIMIT_STATE)?;
        self.nonce = NonZeroU32::new(encrypt_nonce).unwrap_or_else(|| unreachable!());

        let mut ratelimit_state_data = ratelimit_state_data.add(query_phones)?.into_inner();

        self.key
            .encrypt(ratelimit_state_data.get_mut(), &[], &self.get_iv(), &mut ratelimit_state_mac)?;

        Ok((ratelimit_state_data.into_inner(), ratelimit_state_mac))
    }
}

impl Default for RatelimitState {
    fn default() -> Self {
        Self {
            nonce: NonZeroU32::new(1).unwrap_or_else(|| static_unreachable!()),
            key:   Default::default(),
        }
    }
}

impl RatelimitStateData {
    pub fn new(data: SecretValue<Box<[u8]>>) -> Self {
        Self { data }
    }

    pub fn set_size_limit(&mut self, lower_limit_inclusive: u32, range: u32) -> Result<(), SgxStatus> {
        let mut size_limit_rand_data = [0; mem::size_of::<u32>()];

        RdRand.try_fill_bytes(&mut size_limit_rand_data).map_err(|_| SGX_ERROR_UNEXPECTED)?;

        let mut size_limit_rand = CtU64::nan();
        let mut range_ct = CtU64::nan();

        size_limit_rand.set(u32::from_le_bytes(size_limit_rand_data).into());
        range_ct.set(range.into());
        size_limit_rand.rem_assign(&range_ct);

        let size_limit = (u64::from(&size_limit_rand) as u32).saturating_add(lower_limit_inclusive);

        let size_limit_data: &mut [u8; mem::size_of::<u32>()] = self
            .data
            .get_mut()
            .get_mut(..mem::size_of::<u32>())
            .ok_or(CDS_ERROR_INVALID_RATE_LIMIT_STATE)?
            .try_into()
            .unwrap_or_else(|_| static_unreachable!());

        *size_limit_data = u32::to_le_bytes(size_limit);

        Ok(())
    }

    pub fn slot_count(&self) -> u32 {
        let slots_data_len = self.data.get().len() - Self::size_limit_data_len();
        let slot_count_raw = slots_data_len / 8;
        // one quarter of the slots are dummy slots
        (slot_count_raw.saturating_mul(3) / 4) as u32
    }

    pub fn add(mut self, phones: &[u64]) -> Result<Self, SgxStatus> {
        if self.data.get().len() < Self::size_limit_data_len() {
            return Err(CDS_ERROR_INVALID_RATE_LIMIT_STATE);
        }
        let (size_limit_data, slots_data) = self.data.get_mut().split_at_mut(Self::size_limit_data_len());

        ratelimit_set_add(slots_data, phones);

        let size_limit_data: &mut [u8; mem::size_of::<u32>()] = size_limit_data.try_into().unwrap_or_else(|_| static_unreachable!());
        if ratelimit_set_size(slots_data) < u32::from_le_bytes(*size_limit_data) {
            Ok(self)
        } else {
            Err(CDS_ERROR_RATE_LIMIT_EXCEEDED)
        }
    }

    pub fn into_inner(self) -> SecretValue<Box<[u8]>> {
        self.data
    }

    const fn size_limit_data_len() -> usize {
        mem::size_of::<u32>()
    }
}

//
// RequestPhoneList
//

impl<'a> IntoIterator for &'a RequestPhoneList {
    type IntoIter = iter::Map<slice::ChunksExact<'a, u8>, fn(&[u8]) -> Phone>;
    type Item = Phone;

    fn into_iter(self) -> Self::IntoIter {
        let phones_data = self.data.get().get(COMMITMENT_NONCE_SIZE..).unwrap_or_default();
        phones_data
            .chunks_exact(mem::size_of::<Phone>())
            .map(RequestPhoneList::decode_phone)
    }
}

impl RequestPhoneList {
    fn new(data: Box<[u8]>) -> Self {
        Self {
            data: SecretValue::new(data),
        }
    }

    fn iter(&self) -> impl ExactSizeIterator<Item = Phone> + '_ {
        self.into_iter()
    }

    fn decode_phone(data: &[u8]) -> Phone {
        u64::from_ne_bytes(data.try_into().expect("chunks are of size 8"))
    }
}

//
// helpers
//

#[inline(never)]
fn hash_query_phone(phone: &mut u64) {
    // enough to hold 2^64 in decimal
    let mut ascii_phone = SecretValue::new([0u8; 20]);

    let mut divisor = CtU64::nan();
    let mut quotient = CtU64::nan();
    let mut remainder = CtU64::nan();

    divisor.set(10);
    quotient.set(*phone);
    for ascii_digit in ascii_phone.get_mut().iter_mut().rev() {
        quotient.divrem_assign(&divisor, &mut remainder);
        *ascii_digit = ('0' as u8) + (u64::from(&remainder) as u8)
    }

    hash_ascii_phone(phone, ascii_phone.get())
}

#[inline(never)]
fn hash_ascii_phone(phone: &mut u64, ascii_phone: &[u8; 20]) {
    let mut leading_zeroes: Choice = 1.into();
    let mut ascii_phone = &ascii_phone[..];
    while let Some(leading_digit) = ascii_phone.get(0) {
        let leading_zero = leading_digit.ct_eq(&('0' as u8));
        hash_truncated(ascii_phone, phone, leading_zeroes & !leading_zero);
        leading_zeroes &= leading_zero;
        ascii_phone = &ascii_phone[1..];
    }
}

#[inline(never)]
fn hash_truncated(data: &[u8], phone: &mut u64, choice: Choice) {
    let mut hash_context = SHA1Context::default();

    hash_context.update(&['+' as u8]);
    hash_context.update(data);

    let mut hash_result = SecretValue::new([0; SHA1Context::hash_len()]);
    hash_context.result(hash_result.get_mut());
    hash_context.clear();

    let truncated_hash = u64::from_ne_bytes(hash_result.get()[..8].try_into().unwrap_or_else(|_| static_unreachable!()));
    phone.conditional_assign(&truncated_hash, choice);
}

//
// tests
//

#[cfg(test)]
mod tests {
    use std::cell::*;
    use std::ffi::c_void;
    use std::mem;
    use std::rc::*;

    use mockers::matchers::*;
    use mockers::*;

    use super::*;

    pub struct TestQuery {
        pub phone_count:  u32,
        pub data:         Box<[u8]>,
        pub commitment:   [u8; 32],
        pub request_data: Box<[u8]>,
    }

    pub struct TestRatelimitState {
        pub data: Box<[u8]>,
    }

    lazy_static::lazy_static! {
        static ref VALID_IN_PHONES: Vec<Phone> = vec![test_ffi::rand(); 1];
        static ref VALID_IN_UUIDS:  Vec<Uuid>  = vec![Uuid { data64: test_ffi::rand() }; 1];
    }

    fn empty_init_args() -> Box<StartArgs> {
        Box::new(StartArgs {
            max_query_phones:     0,
            max_ratelimit_states: 1,
        })
    }
    fn empty_call_args() -> Box<CallArgs> {
        Box::new(Default::default())
    }
    fn empty_stop_args() -> Box<StopArgs> {
        Box::new(Default::default())
    }
    fn valid_stop_args() -> Box<StopArgs> {
        Box::new(StopArgs {
            in_phones:      VALID_IN_PHONES.as_ptr() as *mut Phone,
            in_uuids:       VALID_IN_UUIDS.as_ptr() as *mut Uuid,
            in_phone_count: 1,
        })
    }

    #[test]
    fn test_in_phones_outside_enclave() {
        let scenario = Scenario::new();
        let sgx_is_outside_enclave = test_ffi::mock_for(&sgx_ffi::mocks::SGX_IS_OUTSIDE_ENCLAVE, &scenario);
        scenario.expect(sgx_is_outside_enclave.sgx_is_outside_enclave(any(), any()).and_return(false));

        let server = SgxsdServerState::init(Some(&empty_init_args())).unwrap();
        server.terminate(Some(&valid_stop_args())).unwrap_err();
    }

    #[test]
    fn test_in_uuids_outside_enclave() {
        let valid_stop_args = valid_stop_args();

        let scenario = Scenario::new();
        let sgx_is_outside_enclave = test_ffi::mock_for(&sgx_ffi::mocks::SGX_IS_OUTSIDE_ENCLAVE, &scenario);
        scenario.expect(
            sgx_is_outside_enclave
                .sgx_is_outside_enclave(valid_stop_args.in_phones as *const c_void, any())
                .and_return(true),
        );
        scenario.expect(
            sgx_is_outside_enclave
                .sgx_is_outside_enclave(valid_stop_args.in_uuids as *const c_void, any())
                .and_return(false),
        );

        let server = SgxsdServerState::init(Some(&empty_init_args())).unwrap();
        server.terminate(Some(&valid_stop_args)).unwrap_err();
    }

    #[test]
    fn test_in_phones_overflow() {
        let scenario = Scenario::new();
        let sgx_is_outside_enclave = test_ffi::mock_for(&sgx_ffi::mocks::SGX_IS_OUTSIDE_ENCLAVE, &scenario);
        scenario.expect(sgx_is_outside_enclave.sgx_is_outside_enclave(any(), any()).never());

        let server = SgxsdServerState::init(Some(&empty_init_args())).unwrap();
        server
            .terminate(Some(&StopArgs {
                in_phones:      VALID_IN_PHONES.as_ptr() as *mut Phone,
                in_uuids:       VALID_IN_UUIDS.as_ptr() as *mut Uuid,
                in_phone_count: 1 + usize::max_value() / mem::size_of::<Phone>(),
            }))
            .unwrap_err();
    }

    #[test]
    fn test_in_uuids_overflow() {
        let scenario = Scenario::new();
        let sgx_is_outside_enclave = test_ffi::mock_for(&sgx_ffi::mocks::SGX_IS_OUTSIDE_ENCLAVE, &scenario);
        scenario.expect(sgx_is_outside_enclave.sgx_is_outside_enclave(any(), any()).never());

        let server = SgxsdServerState::init(Some(&empty_init_args())).unwrap();
        server
            .terminate(Some(&StopArgs {
                in_phones:      VALID_IN_PHONES.as_ptr() as *mut Phone,
                in_uuids:       VALID_IN_UUIDS.as_ptr() as *mut Uuid,
                in_phone_count: 1 + usize::max_value() / mem::size_of::<Uuid>(),
            }))
            .unwrap_err();
    }

    #[test]
    fn test_zero_max_batch() {
        let server = SgxsdServerState::init(Some(&empty_init_args())).unwrap();
        server.terminate(Some(&empty_stop_args())).unwrap();
    }

    #[test]
    fn test_empty_batch() {
        let valid_stop_args = valid_stop_args();

        let scenario = Scenario::new();
        let sgx_is_outside_enclave = test_ffi::mock_for(&sgx_ffi::mocks::SGX_IS_OUTSIDE_ENCLAVE, &scenario);
        scenario.expect(
            sgx_is_outside_enclave
                .sgx_is_outside_enclave(valid_stop_args.in_phones as *const c_void, any())
                .and_return(true),
        );
        scenario.expect(
            sgx_is_outside_enclave
                .sgx_is_outside_enclave(valid_stop_args.in_uuids as *const c_void, any())
                .and_return(true),
        );

        let server = SgxsdServerState::init(Some(&StartArgs {
            max_query_phones:     1,
            max_ratelimit_states: 1,
        }))
        .unwrap();
        server.terminate(Some(&valid_stop_args)).unwrap();
    }

    #[test]
    fn test_empty_msg() {
        let scenario = Scenario::new();
        scenario.expect(
            test_ffi::mock_for(&sgxsd_ffi::mocks::SGXSD_ENCLAVE_SERVER_NOREPLY, &scenario)
                .sgxsd_enclave_server_noreply(any())
                .and_return(SGX_SUCCESS),
        );

        let mut server = SgxsdServerState::init(Some(&StartArgs {
            max_query_phones:     1,
            max_ratelimit_states: 1,
        }))
        .unwrap();
        assert_eq!(
            server
                .handle_call(Some(&empty_call_args()), &[], SgxsdMsgFrom::mock())
                .unwrap_err()
                .0,
            SGX_ERROR_INVALID_PARAMETER
        );
        server.terminate(Some(&empty_stop_args())).unwrap();
    }

    #[test]
    fn test_hash_query_phone_zero() {
        for phone in 1..10 {
            test_hash_query_phone(phone);
        }
    }

    #[test]
    fn test_hash_query_phone_1_digit() {
        for phone in 1..10 {
            test_hash_query_phone(phone);
        }
    }

    #[test]
    fn test_hash_query_phone_2_digit() {
        for phone in 10..100 {
            test_hash_query_phone(phone);
        }
    }

    #[test]
    fn test_hash_query_phone_13_digit() {
        for phone in 1..10 {
            test_hash_query_phone(1_000_000_000_000 * phone);
            test_hash_query_phone(1_000_000_000_000 * phone + 234_567_890_123);
        }
    }

    #[test]
    fn test_hash_query_phone_too_large() {
        test_hash_query_phone((std::u64::MAX >> 1) + 0);
        test_hash_query_phone((std::u64::MAX >> 1) + 1);
        test_hash_query_phone((std::u64::MAX >> 1) + 2);
        test_hash_query_phone(std::u64::MAX - 1);
        test_hash_query_phone(std::u64::MAX - 0);
    }

    fn test_hash_query_phone(mut phone: u64) {
        let scenario = Scenario::new();
        let mock = test_ffi::mock_for(&sgxsd_ffi::mocks::BEARSSL_SHA1, &scenario);

        let is_correct_hash_shared = Rc::new(RefCell::new(false));

        scenario.expect(mock.update(eq(&b"+"[..])).and_return_default().times(20));
        let correct_hash_len = format!("{}", phone).into_bytes().len();
        for len in 1..=20 {
            let expected = format!("{:01$}", phone, len).into_bytes();
            let is_correct_hash_shared = is_correct_hash_shared.clone();
            scenario.expect(
                mock.update(check(move |actual: &&[u8]| **actual == expected[expected.len() - len..]))
                    .and_call(move |actual: &[u8]| {
                        let is_correct_hash = actual.len() == correct_hash_len;
                        assert!(!*is_correct_hash_shared.borrow() || !is_correct_hash);
                        *is_correct_hash_shared.borrow_mut() = is_correct_hash;
                    }),
            );
        }

        let mock_result = test_ffi::rand();
        scenario.expect(
            mock.out()
                .and_call_clone(move || {
                    if *is_correct_hash_shared.borrow() {
                        mock_result
                    } else {
                        Default::default()
                    }
                })
                .times(20),
        );

        hash_query_phone(&mut phone);
        assert_eq!(phone, u64::from_ne_bytes(mock_result[..8].try_into().unwrap()));
    }

    //
    // TestQuery impls
    //

    impl TestQuery {
        pub fn new(phone_count: u32) -> Self {
            let query_data_size = COMMITMENT_NONCE_SIZE + phone_count as usize * mem::size_of::<Phone>();
            Self {
                phone_count,
                data: vec![0; query_data_size].into(),
                commitment: [0; 32],
                request_data: vec![0; SGXSD_AES_GCM_KEY_SIZE as usize].into(),
            }
        }
    }

    //
    // TestRatelimitState impls
    //

    impl TestRatelimitState {
        pub fn new(slot_count: usize) -> Self {
            assert_eq!(slot_count % 4, 0);
            let data_size = mem::size_of::<u32>() + (slot_count * 12) + SGXSD_AES_GCM_MAC_SIZE as usize;
            Self {
                data: vec![0; data_size].into(),
            }
        }

        pub fn slots_data_mut(&mut self) -> &mut [u8] {
            let slots_data_start = mem::size_of::<u32>();
            let slots_data_end = self.data.len() - SGXSD_AES_GCM_MAC_SIZE as usize;
            &mut self.data[slots_data_start..slots_data_end]
        }
    }
}
