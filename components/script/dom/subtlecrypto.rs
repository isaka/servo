/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::num::NonZero;
use std::ptr;
use std::rc::Rc;

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, StreamCipher};
use aes::{Aes128, Aes192, Aes256};
use base64::prelude::*;
use dom_struct::dom_struct;
use js::conversions::ConversionResult;
use js::jsapi::{JSObject, JS_NewObject};
use js::jsval::ObjectValue;
use js::rust::MutableHandleObject;
use js::typedarray::ArrayBufferU8;
use ring::{digest, pbkdf2};
use servo_rand::{RngCore, ServoRng};

use crate::dom::bindings::buffer_source::create_buffer_source;
use crate::dom::bindings::cell::DomRefCell;
use crate::dom::bindings::codegen::Bindings::CryptoKeyBinding::{
    CryptoKeyMethods, KeyType, KeyUsage,
};
use crate::dom::bindings::codegen::Bindings::SubtleCryptoBinding::{
    AesCbcParams, AesCtrParams, AesKeyAlgorithm, AesKeyGenParams, Algorithm, AlgorithmIdentifier,
    JsonWebKey, KeyAlgorithm, KeyFormat, Pbkdf2Params, SubtleCryptoMethods,
};
use crate::dom::bindings::codegen::UnionTypes::{
    ArrayBufferViewOrArrayBuffer, ArrayBufferViewOrArrayBufferOrJsonWebKey,
};
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::import::module::SafeJSContext;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::{Trusted, TrustedPromise};
use crate::dom::bindings::reflector::{reflect_dom_object, DomObject, Reflector};
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::trace::RootedTraceableBox;
use crate::dom::cryptokey::{CryptoKey, Handle};
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::Promise;
use crate::dom::window::Window;
use crate::dom::workerglobalscope::WorkerGlobalScope;
use crate::realms::InRealm;
use crate::script_runtime::{CanGc, JSContext};
use crate::task::TaskCanceller;
use crate::task_source::dom_manipulation::DOMManipulationTaskSource;
use crate::task_source::TaskSource;

// String constants for algorithms/curves
const ALG_AES_CBC: &str = "AES-CBC";
const ALG_AES_CTR: &str = "AES-CTR";
const ALG_AES_GCM: &str = "AES-GCM";
const ALG_AES_KW: &str = "AES-KW";
const ALG_SHA1: &str = "SHA-1";
const ALG_SHA256: &str = "SHA-256";
const ALG_SHA384: &str = "SHA-384";
const ALG_SHA512: &str = "SHA-512";
const ALG_HMAC: &str = "HMAC";
const ALG_HKDF: &str = "HKDF";
const ALG_PBKDF2: &str = "PBKDF2";
const ALG_RSASSA_PKCS1: &str = "RSASSA-PKCS1-v1_5";
const ALG_RSA_OAEP: &str = "RSA-OAEP";
const ALG_RSA_PSS: &str = "RSA-PSS";
const ALG_ECDH: &str = "ECDH";
const ALG_ECDSA: &str = "ECDSA";

#[allow(dead_code)]
static SUPPORTED_ALGORITHMS: &[&str] = &[
    ALG_AES_CBC,
    ALG_AES_CTR,
    ALG_AES_GCM,
    ALG_AES_KW,
    ALG_SHA1,
    ALG_SHA256,
    ALG_SHA384,
    ALG_SHA512,
    ALG_HMAC,
    ALG_HKDF,
    ALG_PBKDF2,
    ALG_RSASSA_PKCS1,
    ALG_RSA_OAEP,
    ALG_RSA_PSS,
    ALG_ECDH,
    ALG_ECDSA,
];

const NAMED_CURVE_P256: &str = "P-256";
const NAMED_CURVE_P384: &str = "P-384";
const NAMED_CURVE_P521: &str = "P-521";
#[allow(dead_code)]
static SUPPORTED_CURVES: &[&str] = &[NAMED_CURVE_P256, NAMED_CURVE_P384, NAMED_CURVE_P521];

type Aes128CbcEnc = cbc::Encryptor<Aes128>;
type Aes128CbcDec = cbc::Decryptor<Aes128>;
type Aes192CbcEnc = cbc::Encryptor<Aes192>;
type Aes192CbcDec = cbc::Decryptor<Aes192>;
type Aes256CbcEnc = cbc::Encryptor<Aes256>;
type Aes256CbcDec = cbc::Decryptor<Aes256>;
type Aes128Ctr = ctr::Ctr64BE<Aes128>;
type Aes192Ctr = ctr::Ctr64BE<Aes192>;
type Aes256Ctr = ctr::Ctr64BE<Aes256>;

#[dom_struct]
pub struct SubtleCrypto {
    reflector_: Reflector,
    #[no_trace]
    rng: DomRefCell<ServoRng>,
}

impl SubtleCrypto {
    fn new_inherited() -> SubtleCrypto {
        SubtleCrypto {
            reflector_: Reflector::new(),
            rng: DomRefCell::new(ServoRng::default()),
        }
    }

    pub(crate) fn new(global: &GlobalScope) -> DomRoot<SubtleCrypto> {
        reflect_dom_object(Box::new(SubtleCrypto::new_inherited()), global)
    }

    fn task_source_with_canceller(&self) -> (DOMManipulationTaskSource, TaskCanceller) {
        if let Some(window) = self.global().downcast::<Window>() {
            window
                .task_manager()
                .dom_manipulation_task_source_with_canceller()
        } else if let Some(worker_global) = self.global().downcast::<WorkerGlobalScope>() {
            let task_source = worker_global.dom_manipulation_task_source();
            let canceller = worker_global.task_canceller();
            (task_source, canceller)
        } else {
            unreachable!("Couldn't downcast to Window or WorkerGlobalScope!");
        }
    }
}

impl SubtleCryptoMethods for SubtleCrypto {
    /// <https://w3c.github.io/webcrypto/#SubtleCrypto-method-encrypt>
    fn Encrypt(
        &self,
        cx: JSContext,
        algorithm: AlgorithmIdentifier,
        key: &CryptoKey,
        data: ArrayBufferViewOrArrayBuffer,
        comp: InRealm,
        can_gc: CanGc,
    ) -> Rc<Promise> {
        let normalized_algorithm = normalize_algorithm(cx, &algorithm, "encrypt");
        let promise = Promise::new_in_current_realm(comp, can_gc);
        let data = match data {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => view.to_vec(),
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => buffer.to_vec(),
        };

        let (task_source, canceller) = self.task_source_with_canceller();
        let this = Trusted::new(self);
        let trusted_promise = TrustedPromise::new(promise.clone());
        let trusted_key = Trusted::new(key);
        let alg = normalized_algorithm.clone();
        let key_alg = key.algorithm();
        let valid_usage = key.usages().contains(&KeyUsage::Encrypt);
        let _ = task_source.queue_with_canceller(
            task!(encrypt: move || {
                let subtle = this.root();
                let promise = trusted_promise.root();
                let key = trusted_key.root();
                let cx = GlobalScope::get_cx();
                rooted!(in(*cx) let mut array_buffer_ptr = ptr::null_mut::<JSObject>());
                let text = match alg {
                    Ok(NormalizedAlgorithm::AesCbcParams(key_gen_params)) => {
                        if !valid_usage || key_gen_params.name != key_alg {
                            Err(Error::InvalidAccess)
                        } else {
                            match subtle.encrypt_aes_cbc(
                                key_gen_params, &key, &data, cx, array_buffer_ptr.handle_mut()
                            ) {
                                Ok(_) => Ok(array_buffer_ptr.handle()),
                                Err(e) => Err(e),
                            }
                        }
                    },
                    Ok(NormalizedAlgorithm::AesCtrParams(key_gen_params)) => {
                        if !valid_usage || key_gen_params.name != key_alg {
                            Err(Error::InvalidAccess)
                        } else {
                            match subtle.encrypt_decrypt_aes_ctr(
                                key_gen_params, &key, &data, cx, array_buffer_ptr.handle_mut()
                            ) {
                                Ok(_) => Ok(array_buffer_ptr.handle()),
                                Err(e) => Err(e),
                            }
                        }
                    },
                    _ => Err(Error::NotSupported),
                };
                match text {
                    Ok(text) => promise.resolve_native(&*text),
                    Err(e) => promise.reject_error(e),
                }
            }),
            &canceller,
        );

        promise
    }

    /// <https://w3c.github.io/webcrypto/#SubtleCrypto-method-decrypt>
    fn Decrypt(
        &self,
        cx: JSContext,
        algorithm: AlgorithmIdentifier,
        key: &CryptoKey,
        data: ArrayBufferViewOrArrayBuffer,
        comp: InRealm,
        can_gc: CanGc,
    ) -> Rc<Promise> {
        let normalized_algorithm = normalize_algorithm(cx, &algorithm, "decrypt");
        let promise = Promise::new_in_current_realm(comp, can_gc);
        let data = match data {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => view.to_vec(),
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => buffer.to_vec(),
        };

        let (task_source, canceller) = self.task_source_with_canceller();
        let this = Trusted::new(self);
        let trusted_promise = TrustedPromise::new(promise.clone());
        let trusted_key = Trusted::new(key);
        let alg = normalized_algorithm.clone();
        let key_alg = key.algorithm();
        let valid_usage = key.usages().contains(&KeyUsage::Decrypt);
        let _ = task_source.queue_with_canceller(
            task!(decrypt: move || {
                let subtle = this.root();
                let promise = trusted_promise.root();
                let key = trusted_key.root();
                let cx = GlobalScope::get_cx();
                rooted!(in(*cx) let mut array_buffer_ptr = ptr::null_mut::<JSObject>());
                let text = match alg {
                    Ok(NormalizedAlgorithm::AesCbcParams(key_gen_params)) => {
                        if !valid_usage || key_gen_params.name != key_alg {
                            Err(Error::InvalidAccess)
                        } else {
                            match subtle.decrypt_aes_cbc(
                                key_gen_params, &key, &data, cx, array_buffer_ptr.handle_mut()
                            ) {
                                Ok(_) => Ok(array_buffer_ptr.handle()),
                                Err(e) => Err(e),
                            }
                        }
                    },
                    Ok(NormalizedAlgorithm::AesCtrParams(key_gen_params)) => {
                        if !valid_usage || key_gen_params.name != key_alg {
                            Err(Error::InvalidAccess)
                        } else {
                            match subtle.encrypt_decrypt_aes_ctr(
                                key_gen_params, &key, &data, cx, array_buffer_ptr.handle_mut()
                            ) {
                                Ok(_) => Ok(array_buffer_ptr.handle()),
                                Err(e) => Err(e),
                            }
                        }
                    },
                    _ => Err(Error::NotSupported),
                };
                match text {
                    Ok(text) => promise.resolve_native(&*text),
                    Err(e) => promise.reject_error(e),
                }
            }),
            &canceller,
        );

        promise
    }

    /// <https://w3c.github.io/webcrypto/#SubtleCrypto-method-digest>
    fn Digest(
        &self,
        cx: SafeJSContext,
        algorithm: AlgorithmIdentifier,
        data: ArrayBufferViewOrArrayBuffer,
        comp: InRealm,
        can_gc: CanGc,
    ) -> Rc<Promise> {
        // Step 1. Let algorithm be the algorithm parameter passed to the digest() method.

        // Step 2. Let data be the result of getting a copy of the bytes held by the
        // data parameter passed to the digest() method.
        let data = match data {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => view.to_vec(),
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => buffer.to_vec(),
        };

        // Step 3. Let normalizedAlgorithm be the result of normalizing an algorithm,
        // with alg set to algorithm and op set to "digest".
        let promise = Promise::new_in_current_realm(comp, can_gc);
        let normalized_algorithm = match normalize_algorithm(cx, &algorithm, "digest") {
            Ok(normalized_algorithm) => normalized_algorithm,
            Err(e) => {
                // Step 4. If an error occurred, return a Promise rejected with normalizedAlgorithm.
                promise.reject_error(e);
                return promise;
            },
        };

        // Step 5. Let promise be a new Promise.
        // NOTE: We did that in preparation of Step 4.

        // Step 6. Return promise and perform the remaining steps in parallel.
        let (task_source, canceller) = self.task_source_with_canceller();
        let trusted_promise = TrustedPromise::new(promise.clone());
        let alg = normalized_algorithm.clone();

        let _ = task_source.queue_with_canceller(
            task!(generate_key: move || {
                // Step 7. If the following steps or referenced procedures say to throw an error, reject promise
                // with the returned error and then terminate the algorithm.
                let promise = trusted_promise.root();

                // Step 8. Let result be the result of performing the digest operation specified by
                // normalizedAlgorithm using algorithm, with data as message.
                let digest = match alg.digest(&data) {
                    Ok(digest) => digest,
                    Err(e) => {
                        promise.reject_error(e);
                        return;
                    }
                };

                let cx = GlobalScope::get_cx();
                rooted!(in(*cx) let mut array_buffer_ptr = ptr::null_mut::<JSObject>());
                create_buffer_source::<ArrayBufferU8>(cx, digest.as_ref(), array_buffer_ptr.handle_mut())
                    .expect("failed to create buffer source for exported key.");


                // Step 9. Resolve promise with result.
                promise.resolve_native(&*array_buffer_ptr);
            }),
            &canceller,
        );

        promise
    }

    /// <https://w3c.github.io/webcrypto/#SubtleCrypto-method-generateKey>
    fn GenerateKey(
        &self,
        cx: JSContext,
        algorithm: AlgorithmIdentifier,
        extractable: bool,
        key_usages: Vec<KeyUsage>,
        comp: InRealm,
        can_gc: CanGc,
    ) -> Rc<Promise> {
        let normalized_algorithm = normalize_algorithm(cx, &algorithm, "generateKey");
        let promise = Promise::new_in_current_realm(comp, can_gc);
        if let Err(e) = normalized_algorithm {
            promise.reject_error(e);
            return promise;
        }

        let (task_source, canceller) = self.task_source_with_canceller();
        let this = Trusted::new(self);
        let trusted_promise = TrustedPromise::new(promise.clone());
        let alg = normalized_algorithm.clone();
        let _ = task_source.queue_with_canceller(
            task!(generate_key: move || {
                let subtle = this.root();
                let promise = trusted_promise.root();
                let key = match alg {
                    Ok(NormalizedAlgorithm::AesKeyGenParams(key_gen_params)) => {
                        subtle.generate_key_aes(key_usages, key_gen_params, extractable)
                    },
                    _ => Err(Error::NotSupported),
                };
                match key {
                    Ok(key) => promise.resolve_native(&key),
                    Err(e) => promise.reject_error(e),
                }
            }),
            &canceller,
        );

        promise
    }

    /// <https://w3c.github.io/webcrypto/#dfn-SubtleCrypto-method-deriveBits>
    fn DeriveBits(
        &self,
        cx: SafeJSContext,
        algorithm: AlgorithmIdentifier,
        base_key: &CryptoKey,
        length: Option<u32>,
        comp: InRealm,
        can_gc: CanGc,
    ) -> Rc<Promise> {
        // Step 1.  Let algorithm, baseKey and length, be the algorithm, baseKey and
        // length parameters passed to the deriveBits() method, respectively.

        // Step 2. Let normalizedAlgorithm be the result of normalizing an algorithm,
        // with alg set to algorithm and op set to "deriveBits".
        let promise = Promise::new_in_current_realm(comp, can_gc);
        let normalized_algorithm = match normalize_algorithm(cx, &algorithm, "deriveBits") {
            Ok(algorithm) => algorithm,
            Err(e) => {
                // Step 3. If an error occurred, return a Promise rejected with normalizedAlgorithm.
                promise.reject_error(e);
                return promise;
            },
        };

        // Step 4. Let promise be a new Promise object.
        // NOTE: We did that in preparation of Step 3.

        // Step 5. Return promise and perform the remaining steps in parallel.
        let (task_source, canceller) = self.task_source_with_canceller();
        let trusted_promise = TrustedPromise::new(promise.clone());
        let trusted_base_key = Trusted::new(base_key);

        let _ = task_source.queue_with_canceller(
            task!(import_key: move || {
                // Step 6. If the following steps or referenced procedures say to throw an error,
                // reject promise with the returned error and then terminate the algorithm.

                // TODO Step 7. If the name member of normalizedAlgorithm is not equal to the name attribute
                // of the [[algorithm]] internal slot of baseKey then throw an InvalidAccessError.
                let promise = trusted_promise.root();
                let base_key = trusted_base_key.root();

                // Step 8. If the [[usages]] internal slot of baseKey does not contain an entry that
                // is "deriveBits", then throw an InvalidAccessError.
                if !base_key.usages().contains(&KeyUsage::DeriveBits) {
                    promise.reject_error(Error::InvalidAccess);
                    return;
                }

                // Step 9. Let result be the result of creating an ArrayBuffer containing the result of performing the
                // derive bits operation specified by normalizedAlgorithm using baseKey, algorithm and length.
                let cx = GlobalScope::get_cx();
                rooted!(in(*cx) let mut array_buffer_ptr = ptr::null_mut::<JSObject>());
                let result = match normalized_algorithm.derive_bits(&base_key, length) {
                    Ok(derived_bits) => derived_bits,
                    Err(e) => {
                        promise.reject_error(e);
                        return;
                    }
                };

                create_buffer_source::<ArrayBufferU8>(cx, &result, array_buffer_ptr.handle_mut())
                    .expect("failed to create buffer source for derived bits.");

                // Step 10. Resolve promise with result.
                promise.resolve_native(&*array_buffer_ptr);
            }),
            &canceller,
        );

        promise
    }

    /// <https://w3c.github.io/webcrypto/#SubtleCrypto-method-importKey>
    fn ImportKey(
        &self,
        cx: JSContext,
        format: KeyFormat,
        key_data: ArrayBufferViewOrArrayBufferOrJsonWebKey,
        algorithm: AlgorithmIdentifier,
        extractable: bool,
        key_usages: Vec<KeyUsage>,
        comp: InRealm,
        can_gc: CanGc,
    ) -> Rc<Promise> {
        let promise = Promise::new_in_current_realm(comp, can_gc);
        let normalized_algorithm = match normalize_algorithm(cx, &algorithm, "importKey") {
            Ok(algorithm) => algorithm,
            Err(e) => {
                promise.reject_error(e);
                return promise;
            },
        };

        // TODO: Figure out a way to Send this data so per-algorithm JWK checks can happen
        let data = match key_data {
            ArrayBufferViewOrArrayBufferOrJsonWebKey::ArrayBufferView(view) => view.to_vec(),
            ArrayBufferViewOrArrayBufferOrJsonWebKey::JsonWebKey(json_web_key) => {
                if let Some(mut data_string) = json_web_key.k {
                    while data_string.len() % 4 != 0 {
                        data_string.push_str("=");
                    }
                    match BASE64_STANDARD.decode(data_string.to_string()) {
                        Ok(data) => data,
                        Err(_) => {
                            promise.reject_error(Error::Syntax);
                            return promise;
                        },
                    }
                } else {
                    promise.reject_error(Error::Syntax);
                    return promise;
                }
            },
            ArrayBufferViewOrArrayBufferOrJsonWebKey::ArrayBuffer(array_buffer) => {
                array_buffer.to_vec()
            },
        };

        let (task_source, canceller) = self.task_source_with_canceller();
        let this = Trusted::new(self);
        let trusted_promise = TrustedPromise::new(promise.clone());
        let _ = task_source.queue_with_canceller(
            task!(import_key: move || {
                let subtle = this.root();
                let promise = trusted_promise.root();
                let imported_key = normalized_algorithm.import_key(&subtle, format, &data, extractable, key_usages);
                match imported_key {
                    Ok(k) => promise.resolve_native(&k),
                    Err(e) => promise.reject_error(e),
                };
            }),
            &canceller,
        );

        promise
    }

    /// <https://w3c.github.io/webcrypto/#SubtleCrypto-method-exportKey>
    fn ExportKey(
        &self,
        format: KeyFormat,
        key: &CryptoKey,
        comp: InRealm,
        can_gc: CanGc,
    ) -> Rc<Promise> {
        let promise = Promise::new_in_current_realm(comp, can_gc);

        let (task_source, canceller) = self.task_source_with_canceller();
        let this = Trusted::new(self);
        let trusted_key = Trusted::new(key);
        let trusted_promise = TrustedPromise::new(promise.clone());
        let _ = task_source.queue_with_canceller(
            task!(export_key: move || {
                let subtle = this.root();
                let promise = trusted_promise.root();
                let key = trusted_key.root();
                let alg_name = key.algorithm();
                if matches!(
                    alg_name.as_str(), ALG_SHA1 | ALG_SHA256 | ALG_SHA384 | ALG_SHA512 | ALG_HKDF | ALG_PBKDF2
                ) {
                    promise.reject_error(Error::NotSupported);
                    return;
                }
                if !key.Extractable() {
                    promise.reject_error(Error::InvalidAccess);
                    return;
                }
                let exported_key = match alg_name.as_str() {
                    ALG_AES_CBC | ALG_AES_CTR => subtle.export_key_aes(format, &key),
                    _ => Err(Error::NotSupported),
                };
                match exported_key {
                    Ok(k) => {
                        match k {
                            AesExportedKey::Raw(k) => {
                                let cx = GlobalScope::get_cx();
                                rooted!(in(*cx) let mut array_buffer_ptr = ptr::null_mut::<JSObject>());
                                create_buffer_source::<ArrayBufferU8>(cx, &k, array_buffer_ptr.handle_mut())
                                    .expect("failed to create buffer source for exported key.");
                                promise.resolve_native(&array_buffer_ptr.get())
                            },
                            AesExportedKey::Jwk(k) => {
                                promise.resolve_native(&k)
                            },
                        }
                    },
                    Err(e) => promise.reject_error(e),
                }
            }),
            &canceller,
        );

        promise
    }
}

#[derive(Clone, Debug)]
pub enum NormalizedAlgorithm {
    #[allow(dead_code)]
    Algorithm(SubtleAlgorithm),
    AesCbcParams(SubtleAesCbcParams),
    AesCtrParams(SubtleAesCtrParams),
    AesKeyGenParams(SubtleAesKeyGenParams),
    Pbkdf2Params(SubtlePbkdf2Params),

    /// <https://w3c.github.io/webcrypto/#sha>
    Sha1,

    /// <https://w3c.github.io/webcrypto/#sha>
    Sha256,

    /// <https://w3c.github.io/webcrypto/#sha>
    Sha384,

    /// <https://w3c.github.io/webcrypto/#sha>
    Sha512,
}

// These "subtle" structs are proxies for the codegen'd dicts which don't hold a DOMString
// so they can be sent safely when running steps in parallel.

#[derive(Clone, Debug)]
pub struct SubtleAlgorithm {
    #[allow(dead_code)]
    pub name: String,
}

impl From<DOMString> for SubtleAlgorithm {
    fn from(name: DOMString) -> Self {
        SubtleAlgorithm {
            name: name.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubtleAesCbcParams {
    #[allow(dead_code)]
    pub name: String,
    pub iv: Vec<u8>,
}

impl From<RootedTraceableBox<AesCbcParams>> for SubtleAesCbcParams {
    fn from(params: RootedTraceableBox<AesCbcParams>) -> Self {
        let iv = match &params.iv {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => view.to_vec(),
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => buffer.to_vec(),
        };
        SubtleAesCbcParams {
            name: params.parent.name.to_string(),
            iv,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubtleAesCtrParams {
    pub name: String,
    pub counter: Vec<u8>,
    pub length: u8,
}

impl From<RootedTraceableBox<AesCtrParams>> for SubtleAesCtrParams {
    fn from(params: RootedTraceableBox<AesCtrParams>) -> Self {
        let counter = match &params.counter {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => view.to_vec(),
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => buffer.to_vec(),
        };
        SubtleAesCtrParams {
            name: params.parent.name.to_string(),
            counter,
            length: params.length,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubtleAesKeyGenParams {
    pub name: String,
    pub length: u16,
}

impl From<AesKeyGenParams> for SubtleAesKeyGenParams {
    fn from(params: AesKeyGenParams) -> Self {
        SubtleAesKeyGenParams {
            name: params.parent.name.to_string().to_uppercase(),
            length: params.length,
        }
    }
}

/// <https://w3c.github.io/webcrypto/#dfn-Pbkdf2Params>
#[derive(Clone, Debug)]
pub struct SubtlePbkdf2Params {
    /// <https://w3c.github.io/webcrypto/#dfn-Pbkdf2Params-salt>
    salt: Vec<u8>,

    /// <https://w3c.github.io/webcrypto/#dfn-Pbkdf2Params-iterations>
    iterations: u32,

    /// <https://w3c.github.io/webcrypto/#dfn-Pbkdf2Params-hash>
    hash: Box<NormalizedAlgorithm>,
}

impl SubtlePbkdf2Params {
    fn new(cx: JSContext, params: RootedTraceableBox<Pbkdf2Params>) -> Fallible<Self> {
        let salt = match &params.salt {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => view.to_vec(),
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => buffer.to_vec(),
        };

        let params = Self {
            salt,
            iterations: params.iterations,
            hash: Box::new(normalize_algorithm(cx, &params.hash, "digest")?),
        };

        Ok(params)
    }
}

/// <https://w3c.github.io/webcrypto/#algorithm-normalization-normalize-an-algorithm>
#[allow(unsafe_code)]
fn normalize_algorithm(
    cx: JSContext,
    algorithm: &AlgorithmIdentifier,
    operation: &str,
) -> Result<NormalizedAlgorithm, Error> {
    match algorithm {
        AlgorithmIdentifier::String(name) => {
            Ok(NormalizedAlgorithm::Algorithm(name.clone().into()))
        },
        AlgorithmIdentifier::Object(obj) => {
            rooted!(in(*cx) let value = ObjectValue(unsafe { *obj.get_unsafe() }));
            let Ok(ConversionResult::Success(algorithm)) = Algorithm::new(cx, value.handle())
            else {
                return Err(Error::Syntax);
            };
            let normalized_name = algorithm.name.str().to_uppercase();

            // This implements the table from https://w3c.github.io/webcrypto/#algorithm-overview
            let normalized_algorithm = match (normalized_name.as_str(), operation) {
                (ALG_AES_CBC, "encrypt") | (ALG_AES_CBC, "decrypt") => {
                    let params_result =
                        AesCbcParams::new(cx, value.handle()).map_err(|_| Error::Operation)?;
                    let ConversionResult::Success(params) = params_result else {
                        return Err(Error::Syntax);
                    };
                    NormalizedAlgorithm::AesCbcParams(params.into())
                },
                (ALG_AES_CTR, "encrypt") | (ALG_AES_CTR, "decrypt") => {
                    let params_result =
                        AesCtrParams::new(cx, value.handle()).map_err(|_| Error::Operation)?;
                    let ConversionResult::Success(params) = params_result else {
                        return Err(Error::Syntax);
                    };
                    NormalizedAlgorithm::AesCtrParams(params.into())
                },
                (ALG_AES_CBC, "generateKey") | (ALG_AES_CTR, "generateKey") => {
                    let params_result =
                        AesKeyGenParams::new(cx, value.handle()).map_err(|_| Error::Operation)?;
                    let ConversionResult::Success(params) = params_result else {
                        return Err(Error::Syntax);
                    };
                    NormalizedAlgorithm::AesKeyGenParams(params.into())
                },
                (ALG_ECDSA, "deriveBits") => NormalizedAlgorithm::Algorithm(SubtleAlgorithm {
                    name: ALG_ECDSA.to_string(),
                }),
                (ALG_HKDF, "deriveBits") => NormalizedAlgorithm::Algorithm(SubtleAlgorithm {
                    name: ALG_HKDF.to_string(),
                }),
                (ALG_PBKDF2, "deriveBits") => {
                    let params_result =
                        Pbkdf2Params::new(cx, value.handle()).map_err(|_| Error::Operation)?;
                    let ConversionResult::Success(params) = params_result else {
                        return Err(Error::Syntax);
                    };
                    let subtle_params = SubtlePbkdf2Params::new(cx, params)?;
                    NormalizedAlgorithm::Pbkdf2Params(subtle_params)
                },
                (ALG_AES_CBC, "importKey") => NormalizedAlgorithm::Algorithm(SubtleAlgorithm {
                    name: ALG_AES_CBC.to_string(),
                }),
                (ALG_AES_CTR, "importKey") => NormalizedAlgorithm::Algorithm(SubtleAlgorithm {
                    name: ALG_AES_CTR.to_string(),
                }),
                (ALG_PBKDF2, "importKey") => NormalizedAlgorithm::Algorithm(SubtleAlgorithm {
                    name: ALG_PBKDF2.to_string(),
                }),
                (ALG_SHA1, "digest") => NormalizedAlgorithm::Sha1,
                (ALG_SHA256, "digest") => NormalizedAlgorithm::Sha256,
                (ALG_SHA384, "digest") => NormalizedAlgorithm::Sha384,
                (ALG_SHA512, "digest") => NormalizedAlgorithm::Sha512,
                _ => return Err(Error::NotSupported),
            };

            Ok(normalized_algorithm)
        },
    }
}

impl SubtleCrypto {
    /// <https://w3c.github.io/webcrypto/#aes-cbc-operations>
    fn encrypt_aes_cbc(
        &self,
        params: SubtleAesCbcParams,
        key: &CryptoKey,
        data: &[u8],
        cx: JSContext,
        handle: MutableHandleObject,
    ) -> Result<(), Error> {
        if params.iv.len() != 16 {
            return Err(Error::Operation);
        }

        let plaintext = Vec::from(data);
        let iv = GenericArray::from_slice(&params.iv);

        let ct = match key.handle() {
            Handle::Aes128(data) => {
                let key_data = GenericArray::from_slice(data);
                Aes128CbcEnc::new(key_data, iv).encrypt_padded_vec_mut::<Pkcs7>(&plaintext)
            },
            Handle::Aes192(data) => {
                let key_data = GenericArray::from_slice(data);
                Aes192CbcEnc::new(key_data, iv).encrypt_padded_vec_mut::<Pkcs7>(&plaintext)
            },
            Handle::Aes256(data) => {
                let key_data = GenericArray::from_slice(data);
                Aes256CbcEnc::new(key_data, iv).encrypt_padded_vec_mut::<Pkcs7>(&plaintext)
            },
            _ => return Err(Error::Data),
        };

        create_buffer_source::<ArrayBufferU8>(cx, &ct, handle)
            .expect("failed to create buffer source for exported key.");

        Ok(())
    }

    /// <https://w3c.github.io/webcrypto/#aes-cbc-operations>
    fn decrypt_aes_cbc(
        &self,
        params: SubtleAesCbcParams,
        key: &CryptoKey,
        data: &[u8],
        cx: JSContext,
        handle: MutableHandleObject,
    ) -> Result<(), Error> {
        if params.iv.len() != 16 {
            return Err(Error::Operation);
        }

        let mut ciphertext = Vec::from(data);
        let iv = GenericArray::from_slice(&params.iv);

        let plaintext = match key.handle() {
            Handle::Aes128(data) => {
                let key_data = GenericArray::from_slice(data);
                Aes128CbcDec::new(key_data, iv)
                    .decrypt_padded_mut::<Pkcs7>(ciphertext.as_mut_slice())
                    .map_err(|_| Error::Operation)?
            },
            Handle::Aes192(data) => {
                let key_data = GenericArray::from_slice(data);
                Aes192CbcDec::new(key_data, iv)
                    .decrypt_padded_mut::<Pkcs7>(ciphertext.as_mut_slice())
                    .map_err(|_| Error::Operation)?
            },
            Handle::Aes256(data) => {
                let key_data = GenericArray::from_slice(data);
                Aes256CbcDec::new(key_data, iv)
                    .decrypt_padded_mut::<Pkcs7>(ciphertext.as_mut_slice())
                    .map_err(|_| Error::Operation)?
            },
            _ => return Err(Error::Data),
        };

        create_buffer_source::<ArrayBufferU8>(cx, plaintext, handle)
            .expect("failed to create buffer source for exported key.");

        Ok(())
    }

    /// <https://w3c.github.io/webcrypto/#aes-ctr-operations>
    fn encrypt_decrypt_aes_ctr(
        &self,
        params: SubtleAesCtrParams,
        key: &CryptoKey,
        data: &[u8],
        cx: JSContext,
        handle: MutableHandleObject,
    ) -> Result<(), Error> {
        if params.counter.len() != 16 || params.length == 0 || params.length > 128 {
            return Err(Error::Operation);
        }

        let mut ciphertext = Vec::from(data);
        let counter = GenericArray::from_slice(&params.counter);

        match key.handle() {
            Handle::Aes128(data) => {
                let key_data = GenericArray::from_slice(data);
                Aes128Ctr::new(key_data, counter).apply_keystream(&mut ciphertext)
            },
            Handle::Aes192(data) => {
                let key_data = GenericArray::from_slice(data);
                Aes192Ctr::new(key_data, counter).apply_keystream(&mut ciphertext)
            },
            Handle::Aes256(data) => {
                let key_data = GenericArray::from_slice(data);
                Aes256Ctr::new(key_data, counter).apply_keystream(&mut ciphertext)
            },
            _ => return Err(Error::Data),
        };

        create_buffer_source::<ArrayBufferU8>(cx, &ciphertext, handle)
            .expect("failed to create buffer source for exported key.");

        Ok(())
    }

    /// <https://w3c.github.io/webcrypto/#aes-cbc-operations>
    /// <https://w3c.github.io/webcrypto/#aes-ctr-operations>
    #[allow(unsafe_code)]
    fn generate_key_aes(
        &self,
        usages: Vec<KeyUsage>,
        key_gen_params: SubtleAesKeyGenParams,
        extractable: bool,
    ) -> Result<DomRoot<CryptoKey>, Error> {
        let mut rand = vec![0; key_gen_params.length as usize];
        self.rng.borrow_mut().fill_bytes(&mut rand);
        let handle = match key_gen_params.length {
            128 => Handle::Aes128(rand),
            192 => Handle::Aes192(rand),
            256 => Handle::Aes256(rand),
            _ => return Err(Error::Operation),
        };

        if usages.iter().any(|usage| {
            !matches!(
                usage,
                KeyUsage::Encrypt | KeyUsage::Decrypt | KeyUsage::WrapKey | KeyUsage::UnwrapKey
            )
        }) || usages.is_empty()
        {
            return Err(Error::Syntax);
        }

        let name = match key_gen_params.name.as_str() {
            ALG_AES_CBC => DOMString::from(ALG_AES_CBC),
            ALG_AES_CTR => DOMString::from(ALG_AES_CTR),
            _ => return Err(Error::NotSupported),
        };

        let cx = GlobalScope::get_cx();
        rooted!(in(*cx) let mut algorithm_object = unsafe {JS_NewObject(*cx, ptr::null()) });
        assert!(!algorithm_object.is_null());

        AesKeyAlgorithm::from_name_and_size(
            name.clone(),
            key_gen_params.length,
            algorithm_object.handle_mut(),
            cx,
        );

        let crypto_key = CryptoKey::new(
            &self.global(),
            KeyType::Secret,
            extractable,
            name,
            algorithm_object.handle(),
            usages,
            handle,
        );

        Ok(crypto_key)
    }

    /// <https://w3c.github.io/webcrypto/#aes-cbc-operations>
    /// <https://w3c.github.io/webcrypto/#aes-ctr-operations>
    #[allow(unsafe_code)]
    fn import_key_aes(
        &self,
        format: KeyFormat,
        data: &[u8],
        extractable: bool,
        usages: Vec<KeyUsage>,
        alg_name: &str,
    ) -> Result<DomRoot<CryptoKey>, Error> {
        if usages.iter().any(|usage| {
            !matches!(
                usage,
                KeyUsage::Encrypt | KeyUsage::Decrypt | KeyUsage::WrapKey | KeyUsage::UnwrapKey
            )
        }) || usages.is_empty()
        {
            return Err(Error::Syntax);
        }
        if !matches!(format, KeyFormat::Raw | KeyFormat::Jwk) {
            return Err(Error::NotSupported);
        }
        let handle = match data.len() * 8 {
            128 => Handle::Aes128(data.to_vec()),
            192 => Handle::Aes192(data.to_vec()),
            256 => Handle::Aes256(data.to_vec()),
            _ => return Err(Error::Data),
        };

        let name = DOMString::from(alg_name.to_string());

        let cx = GlobalScope::get_cx();
        rooted!(in(*cx) let mut algorithm_object = unsafe {JS_NewObject(*cx, ptr::null()) });
        assert!(!algorithm_object.is_null());

        AesKeyAlgorithm::from_name_and_size(
            name.clone(),
            (data.len() * 8) as u16,
            algorithm_object.handle_mut(),
            cx,
        );
        let crypto_key = CryptoKey::new(
            &self.global(),
            KeyType::Secret,
            extractable,
            name,
            algorithm_object.handle(),
            usages,
            handle,
        );

        Ok(crypto_key)
    }

    /// <https://w3c.github.io/webcrypto/#aes-cbc-operations>
    /// <https://w3c.github.io/webcrypto/#aes-ctr-operations>
    fn export_key_aes(&self, format: KeyFormat, key: &CryptoKey) -> Result<AesExportedKey, Error> {
        match format {
            KeyFormat::Raw => match key.handle() {
                Handle::Aes128(key_data) => Ok(AesExportedKey::Raw(key_data.as_slice().to_vec())),
                Handle::Aes192(key_data) => Ok(AesExportedKey::Raw(key_data.as_slice().to_vec())),
                Handle::Aes256(key_data) => Ok(AesExportedKey::Raw(key_data.as_slice().to_vec())),
                _ => Err(Error::Data),
            },
            KeyFormat::Jwk => {
                let (alg, k) = match key.handle() {
                    Handle::Aes128(key_data) => {
                        data_to_jwk_params(key.algorithm().as_str(), "128", key_data.as_slice())
                    },
                    Handle::Aes192(key_data) => {
                        data_to_jwk_params(key.algorithm().as_str(), "192", key_data.as_slice())
                    },
                    Handle::Aes256(key_data) => {
                        data_to_jwk_params(key.algorithm().as_str(), "256", key_data.as_slice())
                    },
                    _ => return Err(Error::Data),
                };
                let jwk = JsonWebKey {
                    alg: Some(alg),
                    crv: None,
                    d: None,
                    dp: None,
                    dq: None,
                    e: None,
                    ext: Some(key.Extractable()),
                    k: Some(k),
                    key_ops: None,
                    kty: Some(DOMString::from("oct")),
                    n: None,
                    oth: None,
                    p: None,
                    q: None,
                    qi: None,
                    use_: None,
                    x: None,
                    y: None,
                };
                Ok(AesExportedKey::Jwk(Box::new(jwk)))
            },
            _ => Err(Error::NotSupported),
        }
    }

    /// <https://w3c.github.io/webcrypto/#pbkdf2-operations>
    #[allow(unsafe_code)]
    fn import_key_pbkdf2(
        &self,
        format: KeyFormat,
        data: &[u8],
        extractable: bool,
        usages: Vec<KeyUsage>,
    ) -> Result<DomRoot<CryptoKey>, Error> {
        // Step 1. If format is not "raw", throw a NotSupportedError
        if format != KeyFormat::Raw {
            return Err(Error::NotSupported);
        }

        // Step 2. If usages contains a value that is not "deriveKey" or "deriveBits", then throw a SyntaxError.
        if usages
            .iter()
            .any(|usage| !matches!(usage, KeyUsage::DeriveKey | KeyUsage::DeriveBits))
        {
            return Err(Error::Syntax);
        }

        // Step 3. If extractable is not false, then throw a SyntaxError.
        if extractable {
            return Err(Error::Syntax);
        }

        // Step 4. Let key be a new CryptoKey representing keyData.
        // Step 5. Set the [[type]] internal slot of key to "secret".
        // Step 6. Let algorithm be a new KeyAlgorithm object.
        // Step 7. Set the name attribute of algorithm to "PBKDF2".
        // Step 8. Set the [[algorithm]] internal slot of key to algorithm.
        let name = DOMString::from(ALG_PBKDF2);
        let cx = GlobalScope::get_cx();
        rooted!(in(*cx) let mut algorithm_object = unsafe {JS_NewObject(*cx, ptr::null()) });
        assert!(!algorithm_object.is_null());
        KeyAlgorithm::from_name(name.clone(), algorithm_object.handle_mut(), cx);

        let key = CryptoKey::new(
            &self.global(),
            KeyType::Secret,
            extractable,
            name,
            algorithm_object.handle(),
            usages,
            Handle::Pbkdf2(data.to_vec()),
        );

        // Step 9. Return key.
        Ok(key)
    }
}

pub enum AesExportedKey {
    Raw(Vec<u8>),
    Jwk(Box<JsonWebKey>),
}

fn data_to_jwk_params(alg: &str, size: &str, key: &[u8]) -> (DOMString, DOMString) {
    let jwk_alg = match alg {
        ALG_AES_CBC => DOMString::from(format!("A{}CBC", size)),
        ALG_AES_CTR => DOMString::from(format!("A{}CTR", size)),
        _ => unreachable!(),
    };
    let mut data = BASE64_STANDARD.encode(key);
    data.retain(|c| c != '=');
    (jwk_alg, DOMString::from(data))
}

impl KeyAlgorithm {
    /// Fill the object referenced by `out` with an [KeyAlgorithm]
    /// of the specified name and size.
    #[allow(unsafe_code)]
    fn from_name(name: DOMString, out: MutableHandleObject, cx: JSContext) {
        let key_algorithm = Self { name };

        unsafe {
            key_algorithm.to_jsobject(*cx, out);
        }
    }
}

impl AesKeyAlgorithm {
    /// Fill the object referenced by `out` with an [AesKeyAlgorithm]
    /// of the specified name and size.
    #[allow(unsafe_code)]
    fn from_name_and_size(name: DOMString, size: u16, out: MutableHandleObject, cx: JSContext) {
        let key_algorithm = Self {
            parent: KeyAlgorithm { name },
            length: size,
        };

        unsafe {
            key_algorithm.to_jsobject(*cx, out);
        }
    }
}

impl SubtlePbkdf2Params {
    /// <https://w3c.github.io/webcrypto/#pbkdf2-operations>
    fn derive_bits(&self, key: &CryptoKey, length: Option<u32>) -> Result<Vec<u8>, Error> {
        // Step 1. If length is null or zero, or is not a multiple of 8, then throw an OperationError.
        let Some(length) = length else {
            return Err(Error::Operation);
        };
        if length == 0 || length % 8 != 0 {
            return Err(Error::Operation);
        };

        // Step 2. If the iterations member of normalizedAlgorithm is zero, then throw an OperationError.
        let Ok(iterations) = NonZero::<u32>::try_from(self.iterations) else {
            return Err(Error::Operation);
        };

        // Step 3. Let prf be the MAC Generation function described in Section 4 of [FIPS-198-1]
        // using the hash function described by the hash member of normalizedAlgorithm.
        let NormalizedAlgorithm::Algorithm(alg) = &*self.hash else {
            return Err(Error::NotSupported);
        };

        let prf = match alg.name.as_str() {
            ALG_SHA1 => pbkdf2::PBKDF2_HMAC_SHA1,
            ALG_SHA256 => pbkdf2::PBKDF2_HMAC_SHA256,
            ALG_SHA384 => pbkdf2::PBKDF2_HMAC_SHA384,
            ALG_SHA512 => pbkdf2::PBKDF2_HMAC_SHA512,
            _ => return Err(Error::NotSupported),
        };

        // Step 4. Let result be the result of performing the PBKDF2 operation defined in Section 5.2 of [RFC8018] using
        // prf as the pseudo-random function, PRF, the password represented by [[handle]] internal slot of key as
        // the password, P, the contents of the salt attribute of normalizedAlgorithm as the salt, S, the value of
        // the iterations attribute of normalizedAlgorithm as the iteration count, c, and length divided by 8 as the
        // intended key length, dkLen.
        let mut result = vec![0; length as usize / 8];
        pbkdf2::derive(
            prf,
            iterations,
            &self.salt,
            key.handle().as_bytes(),
            &mut result,
        );

        // Step 5. If the key derivation operation fails, then throw an OperationError.
        // TODO: Investigate when key derivation can fail and how ring handles that case
        // (pbkdf2::derive does not return a Result type)

        // Step 6. Return result
        Ok(result)
    }
}

impl NormalizedAlgorithm {
    fn derive_bits(&self, key: &CryptoKey, length: Option<u32>) -> Result<Vec<u8>, Error> {
        match self {
            Self::Pbkdf2Params(pbkdf2_params) => pbkdf2_params.derive_bits(key, length),
            _ => Err(Error::NotSupported),
        }
    }

    fn import_key(
        &self,
        subtle: &SubtleCrypto,
        format: KeyFormat,
        secret: &[u8],
        extractable: bool,
        key_usages: Vec<KeyUsage>,
    ) -> Result<DomRoot<CryptoKey>, Error> {
        let alg = match self {
            Self::Algorithm(name) => name,
            _ => {
                return Err(Error::NotSupported);
            },
        };

        match alg.name.as_str() {
            ALG_AES_CBC => {
                subtle.import_key_aes(format, secret, extractable, key_usages, ALG_AES_CBC)
            },
            ALG_AES_CTR => {
                subtle.import_key_aes(format, secret, extractable, key_usages, ALG_AES_CTR)
            },
            ALG_PBKDF2 => subtle.import_key_pbkdf2(format, secret, extractable, key_usages),
            _ => Err(Error::NotSupported),
        }
    }

    fn digest(&self, data: &[u8]) -> Result<impl AsRef<[u8]>, Error> {
        let algorithm = match self {
            Self::Sha1 => &digest::SHA1_FOR_LEGACY_USE_ONLY,
            Self::Sha256 => &digest::SHA256,
            Self::Sha384 => &digest::SHA384,
            Self::Sha512 => &digest::SHA512,
            _ => {
                return Err(Error::NotSupported);
            },
        };
        Ok(digest::digest(algorithm, data))
    }
}
