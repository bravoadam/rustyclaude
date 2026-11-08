//! Linux only: security keys (WebAuthn) for the sign-in pages. WebKitGTK has no WebAuthn
//! support, so webauthn.js routes navigator.credentials.get/create here and we drive the key
//! with Mozilla's authenticator crate (the one Firefox uses).
//!
//! Security: the origin is taken from the webview's own URL, never from the page, and the
//! relying party id must belong to that origin. A page can only get signatures for itself.

use std::{
    sync::{mpsc::channel, Arc, Mutex},
    thread,
};

use authenticator::{
    authenticatorservice::{AuthenticatorService, RegisterArgs, SignArgs},
    crypto::COSEAlgorithm,
    ctap2::server::{
        AuthenticationExtensionsClientInputs, PublicKeyCredentialDescriptor, PublicKeyCredentialParameters,
        PublicKeyCredentialUserEntity, RelyingParty, ResidentKeyRequirement, Transport, UserVerificationRequirement,
    },
    errors::AuthenticatorError,
    statecallback::StateCallback,
    Pin, StatusPinUv, StatusUpdate,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::Url;

/// Google signs legacy U2F keys in with this App ID; browsers allow it for Google origins.
const GOOGLE_APPID: &str = "https://www.gstatic.com/securitykey/origins.json";

static SERVICE: Mutex<Option<AuthenticatorService>> = Mutex::new(None);

#[derive(Deserialize)]
pub struct Request {
    kind: String,
    options: Value,
    pin: Option<String>,
}

/// Errors come back as "Name: message", where Name is the DOMException the page should see.
type Res<T> = Result<T, String>;

fn err(name: &str, msg: impl std::fmt::Display) -> String {
    format!("{name}: {msg}")
}

fn bytes(v: &Value) -> Res<Vec<u8>> {
    B64.decode(v.as_str().unwrap_or_default()).map_err(|_| err("TypeError", "bad base64url"))
}

fn origin_of(url: &Url) -> Res<(String, String)> {
    if url.scheme() != "https" {
        return Err(err("SecurityError", "security keys need an https page"));
    }
    let host = url.host_str().ok_or_else(|| err("SecurityError", "no host"))?.to_string();
    let origin = match url.port() {
        Some(p) => format!("https://{host}:{p}"),
        None => format!("https://{host}"),
    };
    Ok((origin, host))
}

/// The RP id must be the page's host or a parent domain of it (and not a bare TLD).
fn check_rp_id(rp_id: &str, host: &str) -> Res<()> {
    let ok = rp_id.contains('.') && (host == rp_id || host.ends_with(&format!(".{rp_id}")));
    if ok {
        Ok(())
    } else {
        Err(err("SecurityError", format!("'{rp_id}' is not valid for {host}")))
    }
}

fn app_id_allowed(app_id: &str, origin: &str, host: &str) -> bool {
    app_id == origin
        || app_id.starts_with(&format!("{origin}/"))
        || (app_id == GOOGLE_APPID && (host == "google.com" || host.ends_with(".google.com")))
}

fn descriptors(list: &Value) -> Res<Vec<PublicKeyCredentialDescriptor>> {
    list.as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|d| Ok(PublicKeyCredentialDescriptor { id: bytes(&d["id"])?, transports: vec![Transport::USB] }))
        .collect()
}

fn uv(v: &Value) -> UserVerificationRequirement {
    match v.as_str() {
        Some("required") => UserVerificationRequirement::Required,
        Some("discouraged") => UserVerificationRequirement::Discouraged,
        _ => UserVerificationRequirement::Preferred,
    }
}

fn map_error(e: AuthenticatorError) -> String {
    match e {
        AuthenticatorError::CredentialExcluded => err("InvalidStateError", "this key is already registered"),
        AuthenticatorError::PinError(p) => err("NotAllowedError", format!("PIN problem: {p:?}")),
        AuthenticatorError::CancelledByUser => err("NotAllowedError", "cancelled"),
        other => err("NotAllowedError", other),
    }
}

/// Runs one request on the key and waits for it (call from a blocking thread).
fn run<T: Send + 'static>(
    pin: Option<String>,
    start: impl FnOnce(&mut AuthenticatorService, std::sync::mpsc::Sender<StatusUpdate>, StateCallback<authenticator::Result<T>>) -> authenticator::Result<()>,
) -> Res<T> {
    let (status_tx, status_rx) = channel::<StatusUpdate>();
    let (done_tx, done_rx) = channel();
    // PIN prompts: answer once with the PIN we were given; otherwise report it to the page,
    // which asks the user and retries. Dropping the sender makes the request fail.
    // The library may keep the status channel open after finishing, so this thread is never
    // joined; it reports through `outcome` instead.
    let outcome = Arc::new(Mutex::new(None::<String>));
    let out = outcome.clone();
    thread::spawn(move || {
        let mut pin = pin;
        let set = |m: String| *out.lock().unwrap() = Some(m);
        while let Ok(update) = status_rx.recv() {
            match update {
                StatusUpdate::PinUvError(StatusPinUv::PinRequired(sender)) => match pin.take() {
                    Some(p) => {
                        let _ = sender.send(Pin::new(&p));
                    }
                    None => set(err("PinRequired", "enter the key's PIN")),
                },
                StatusUpdate::PinUvError(StatusPinUv::InvalidPin(_, left)) => {
                    set(err("PinInvalid", left.map(|n| n.to_string()).unwrap_or_default()))
                }
                StatusUpdate::PinUvError(StatusPinUv::PinAuthBlocked | StatusPinUv::PinBlocked) => {
                    set(err("NotAllowedError", "the key's PIN is blocked; unplug it and try again"))
                }
                // Several accounts on the key for this site: use the first, as there's no
                // chooser yet. Sign-ins that name the credential (Google's) never get here.
                StatusUpdate::SelectResultNotice(choose, _) => {
                    let _ = choose.send(Some(0));
                }
                _ => {}
            }
        }
    });
    {
        let mut svc = SERVICE.lock().unwrap();
        if svc.is_none() {
            let mut s = AuthenticatorService::new().map_err(|e| err("NotSupportedError", e))?;
            s.add_u2f_usb_hid_platform_transports();
            *svc = Some(s);
        }
        let s = svc.as_mut().unwrap();
        let _ = s.cancel(); // only one request at a time
        start(s, status_tx, StateCallback::new(Box::new(move |r| {
            let _ = done_tx.send(r);
        })))
        .map_err(map_error)?;
    }
    let result = done_rx.recv().map_err(|_| err("NotAllowedError", "the request ended"))?;
    let pin_outcome = outcome.lock().unwrap().take();
    match (result, pin_outcome) {
        (Ok(v), _) => Ok(v),
        (Err(_), Some(pin_msg)) => Err(pin_msg),
        (Err(e), None) => Err(map_error(e)),
    }
}

pub fn cancel() {
    if let Some(s) = SERVICE.lock().unwrap().as_mut() {
        let _ = s.cancel();
    }
}

fn timeout_ms(o: &Value) -> u64 {
    o["timeout"].as_u64().unwrap_or(60_000).clamp(10_000, 180_000)
}

pub fn handle(url: Url, req: Request) -> Res<Value> {
    let (origin, host) = origin_of(&url)?;
    let o = &req.options;
    let kind = match req.kind.as_str() {
        "get" => "webauthn.get",
        "create" => "webauthn.create",
        _ => return Err(err("NotSupportedError", "unknown request")),
    };
    // clientDataJSON in the browser's exact field order; the server checks it byte for byte.
    let client_data = json!({ "type": kind, "challenge": B64.encode(bytes(&o["challenge"])?), "origin": origin, "crossOrigin": false });
    let client_data = serde_json::to_vec(&client_data).unwrap();
    let hash: [u8; 32] = Sha256::digest(&client_data).into();
    let client_data_b64 = B64.encode(&client_data);

    if kind == "webauthn.get" {
        let rp_id = o["rpId"].as_str().unwrap_or(&host).to_string();
        check_rp_id(&rp_id, &host)?;
        let app_id =
            o["extensions"]["appid"].as_str().filter(|a| app_id_allowed(a, &origin, &host)).map(str::to_string);
        let allow_list = descriptors(&o["allowCredentials"])?;
        let single = (allow_list.len() == 1).then(|| allow_list[0].id.clone());
        let args = SignArgs {
            client_data_hash: hash,
            origin: origin.clone(),
            relying_party_id: rp_id,
            allow_list,
            user_verification_req: uv(&o["userVerification"]),
            user_presence_req: true,
            extensions: AuthenticationExtensionsClientInputs { app_id, ..Default::default() },
            pin: None,
            use_ctap1_fallback: false,
        };
        let t = timeout_ms(o);
        let r = run(req.pin, move |s, st, cb| s.sign(t, args, st, cb))?;
        let id = r
            .assertion
            .credentials
            .map(|c| c.id)
            .or(single)
            .ok_or_else(|| err("NotAllowedError", "the key didn't say which credential it used"))?;
        let mut ext = json!({});
        if let Some(used) = r.extensions.app_id {
            ext["appid"] = json!(used);
        }
        Ok(json!({
            "id": B64.encode(&id),
            "clientDataJSON": client_data_b64,
            "authenticatorData": B64.encode(r.assertion.auth_data.to_vec()),
            "signature": B64.encode(&r.assertion.signature),
            "userHandle": r.assertion.user.map(|u| B64.encode(u.id)),
            "extensions": ext,
        }))
    } else {
        let rp_id = o["rp"]["id"].as_str().unwrap_or(&host).to_string();
        check_rp_id(&rp_id, &host)?;
        let algs: Vec<_> = o["pubKeyCredParams"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|p| COSEAlgorithm::try_from(p["alg"].as_i64()?).ok())
            .map(|alg| PublicKeyCredentialParameters { alg })
            .collect();
        let sel = &o["authenticatorSelection"];
        let resident = match sel["residentKey"].as_str() {
            Some("required") => ResidentKeyRequirement::Required,
            Some("preferred") => ResidentKeyRequirement::Preferred,
            _ if sel["requireResidentKey"].as_bool() == Some(true) => ResidentKeyRequirement::Required,
            _ => ResidentKeyRequirement::Discouraged,
        };
        let args = RegisterArgs {
            client_data_hash: hash,
            relying_party: RelyingParty { id: rp_id, name: o["rp"]["name"].as_str().map(str::to_string) },
            origin,
            user: PublicKeyCredentialUserEntity {
                id: bytes(&o["user"]["id"])?,
                name: o["user"]["name"].as_str().map(str::to_string),
                display_name: o["user"]["displayName"].as_str().map(str::to_string),
            },
            pub_cred_params: if algs.is_empty() {
                vec![PublicKeyCredentialParameters { alg: COSEAlgorithm::ES256 }]
            } else {
                algs
            },
            exclude_list: descriptors(&o["excludeCredentials"])?,
            user_verification_req: uv(&sel["userVerification"]),
            resident_key_req: resident,
            extensions: AuthenticationExtensionsClientInputs::default(),
            pin: None,
            use_ctap1_fallback: false,
        };
        let t = timeout_ms(o);
        let r = run(req.pin, move |s, st, cb| s.register(t, args, st, cb))?;
        let att = &r.att_obj;
        let cred = att
            .auth_data
            .credential_data
            .as_ref()
            .ok_or_else(|| err("NotAllowedError", "the key returned no credential"))?;
        let id = B64.encode(&cred.credential_id);
        let auth_data = att.auth_data.to_vec();
        let att_obj = serde_cbor::to_vec(att).map_err(|e| err("UnknownError", e))?;
        Ok(json!({
            "id": id,
            "clientDataJSON": client_data_b64,
            "attestationObject": B64.encode(att_obj),
            "authenticatorData": B64.encode(auth_data),
            "transports": ["usb"],
            "extensions": {},
        }))
    }
}
