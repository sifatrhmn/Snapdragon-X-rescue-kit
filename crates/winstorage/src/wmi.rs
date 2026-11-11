//! Thin WMI (IWbemServices) helper: connect, query, call methods.

use std::sync::Once;

use windows::core::{Error as WinError, BSTR, HSTRING, PCWSTR, VARIANT};
use windows::Win32::Foundation::{E_POINTER, RPC_E_CHANGED_MODE, S_FALSE};
use windows::Win32::Security::PSECURITY_DESCRIPTOR;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoInitializeSecurity, CoSetProxyBlanket,
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_AUTHN_LEVEL_DEFAULT, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::{VT_EMPTY, VT_NULL};
use windows::Win32::System::Wmi::{
    IEnumWbemClassObject, IWbemClassObject, IWbemContext, IWbemLocator, IWbemServices, WbemLocator,
    WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_GENERIC_FLAG_TYPE, WBEM_INFINITE,
};

/// Errors from WMI operations.
#[derive(Debug, thiserror::Error)]
pub enum WmiError {
    /// A COM / WMI call failed.
    #[error("COM/WMI error: {0}")]
    Com(#[from] WinError),
    /// A WMI method ran but reported a non-zero `ReturnValue`.
    #[error("WMI method failed with code {code}: {message}")]
    Method { code: u32, message: String },
}

/// Input parameter value for [`Wmi::call`].
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    U64(u64),
    U32(u32),
    U16(u16),
    Bool(bool),
    Str(String),
    /// CIM `char16`.
    Char(char),
}

impl Value {
    fn to_variant(&self) -> VARIANT {
        match self {
            // WMI marshals uint64 as a string VARIANT.
            Value::U64(v) => VARIANT::from(BSTR::from(v.to_string())),
            // uint32/uint16 travel as VT_I4.
            Value::U32(v) => VARIANT::from(*v as i32),
            Value::U16(v) => VARIANT::from(i32::from(*v)),
            Value::Bool(v) => VARIANT::from(*v),
            Value::Str(v) => VARIANT::from(BSTR::from(v.as_str())),
            // char16 travels as VT_I2.
            Value::Char(c) => VARIANT::from(*c as u32 as u16 as i16),
        }
    }
}

thread_local! {
    static COM: Result<(), WinError> = {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // RPC_E_CHANGED_MODE: thread already initialised with another model; still usable.
        if hr == RPC_E_CHANGED_MODE || hr == S_FALSE { Ok(()) } else { hr.ok() }
    };
}

static SECURITY: Once = Once::new();

fn init_com() -> Result<(), WinError> {
    COM.with(|r| r.clone())?;
    SECURITY.call_once(|| {
        // Process-wide default security. Fails harmlessly (RPC_E_TOO_LATE) if the host
        // process already set it.
        let _ = unsafe {
            CoInitializeSecurity(
                PSECURITY_DESCRIPTOR::default(),
                -1,
                None,
                None,
                RPC_C_AUTHN_LEVEL_DEFAULT,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
                None,
            )
        };
    });
    Ok(())
}

/// Applies the default NTLM/impersonate proxy blanket to a COM proxy interface.
macro_rules! set_blanket {
    ($proxy:expr) => {
        unsafe {
            CoSetProxyBlanket(
                $proxy,
                RPC_C_AUTHN_WINNT,
                RPC_C_AUTHZ_NONE,
                PCWSTR::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )
        }
    };
}

/// Connection to one WMI namespace.
///
/// # Thread affinity
/// `Wmi` wraps COM interface pointers and is therefore `!Send` and `!Sync`. It must be
/// created and used on the SAME thread that called [`Wmi::connect`] (COM is initialised
/// per thread there) and must never be moved across threads; for example a
/// `Box<dyn ... + Send>` holding one will not compile. Wrappers built on top of it (such
/// as the storage backend) must be constructed on the worker thread that uses them, not
/// built on one thread and sent to another.
pub struct Wmi {
    svc: IWbemServices,
}

/// One WMI object (query result row, or method output parameters).
///
/// # Thread affinity
/// Like [`Wmi`], this wraps a COM interface pointer and is `!Send` and `!Sync`: use it only
/// on the thread that produced it (the thread that called [`Wmi::connect`]), and never move
/// it across threads.
pub struct WmiObject {
    obj: IWbemClassObject,
}

impl Wmi {
    /// Connects to `namespace` (e.g. `root\Microsoft\Windows\Storage`), initialising COM
    /// for the calling thread if needed.
    pub fn connect(namespace: &str) -> Result<Wmi, WmiError> {
        init_com()?;
        let locator: IWbemLocator =
            unsafe { CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)? };
        let svc = unsafe {
            locator.ConnectServer(
                &BSTR::from(namespace),
                &BSTR::new(),
                &BSTR::new(),
                &BSTR::new(),
                0,
                &BSTR::new(),
                None::<&IWbemContext>,
            )?
        };
        set_blanket!(&svc)?;
        Ok(Wmi { svc })
    }

    /// Runs a WQL query and returns all rows.
    pub fn query(&self, wql: &str) -> Result<Vec<WmiObject>, WmiError> {
        let en: IEnumWbemClassObject = unsafe {
            self.svc.ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from(wql),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None::<&IWbemContext>,
            )?
        };
        set_blanket!(&en)?;
        let mut rows = Vec::new();
        loop {
            let mut objs: [Option<IWbemClassObject>; 1] = [None];
            let mut returned = 0u32;
            unsafe { en.Next(WBEM_INFINITE, &mut objs, &mut returned) }.ok()?;
            if returned == 0 {
                break;
            }
            if let Some(obj) = objs[0].take() {
                rows.push(WmiObject { obj });
            }
        }
        Ok(rows)
    }

    /// Calls `method` on `object_path` (an instance path such as `MSFT_Disk.Number=1`,
    /// or a class name for static methods). Returns the output-parameters object.
    /// A non-zero `ReturnValue` becomes [`WmiError::Method`].
    pub fn call(
        &self,
        object_path: &str,
        method: &str,
        inputs: &[(&str, Value)],
    ) -> Result<WmiObject, WmiError> {
        let in_params = if inputs.is_empty() {
            None
        } else {
            let mut class_obj: Option<IWbemClassObject> = None;
            unsafe {
                self.svc.GetObject(
                    &BSTR::from(class_of(object_path)),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None::<&IWbemContext>,
                    Some(&mut class_obj),
                    None,
                )?;
            }
            let class_obj = class_obj.ok_or_else(|| WinError::from(E_POINTER))?;
            let mut in_sig: Option<IWbemClassObject> = None;
            unsafe {
                class_obj.GetMethod(
                    &HSTRING::from(method),
                    0,
                    &mut in_sig,
                    std::ptr::null_mut(),
                )?;
            }
            let in_sig = in_sig.ok_or_else(|| WmiError::Method {
                code: 0,
                message: format!("method {method} takes no input parameters"),
            })?;
            let inst = unsafe { in_sig.SpawnInstance(0)? };
            for (name, value) in inputs {
                let v = value.to_variant();
                unsafe { inst.Put(&HSTRING::from(*name), 0, &v, 0)? };
            }
            Some(inst)
        };

        let mut out: Option<IWbemClassObject> = None;
        unsafe {
            self.svc.ExecMethod(
                &BSTR::from(object_path),
                &BSTR::from(method),
                WBEM_GENERIC_FLAG_TYPE(0),
                None::<&IWbemContext>,
                in_params.as_ref(),
                Some(&mut out),
                None,
            )?;
        }
        let out = out.ok_or_else(|| WmiError::Method {
            code: 0,
            message: format!("method {method} returned no output object"),
        })?;
        let out = WmiObject { obj: out };
        out.check_return_value("ReturnValue", method, object_path)?;
        Ok(out)
    }
}

/// True for a VT_NULL / VT_EMPTY variant (a WMI property that is present but has no value).
fn is_null_or_empty(v: &VARIANT) -> bool {
    let vt = unsafe { v.as_raw().Anonymous.Anonymous.vt };
    vt == VT_EMPTY.0 || vt == VT_NULL.0
}

fn variant_u32(v: &VARIANT) -> Result<u32, WinError> {
    // uint32 values above i32::MAX arrive as negative VT_I4.
    u32::try_from(v).or_else(|e| i32::try_from(v).map(|i| i as u32).map_err(|_| e))
}

/// Extracts the class name from a WMI object path.
/// `MSFT_Disk.Number=1` -> `MSFT_Disk`; a full path with a namespace prefix and a quoted key
/// -> `MSFT_Disk`; a bare class name is returned unchanged.
fn class_of(path: &str) -> &str {
    let head = match path.find('=') {
        Some(eq) => {
            let h = &path[..eq];
            &h[..h.rfind('.').unwrap_or(h.len())]
        }
        None => path,
    };
    head.rsplit(':').next().unwrap_or(head)
}

impl WmiObject {
    fn variant(&self, name: &str) -> Option<VARIANT> {
        let mut v = VARIANT::default();
        unsafe {
            self.obj
                .Get(&HSTRING::from(name), 0, &mut v, None, None)
                .ok()?;
        }
        Some(v)
    }

    pub fn get_u64(&self, n: &str) -> Option<u64> {
        // uint64 properties arrive as VT_BSTR; the conversion parses them.
        u64::try_from(&self.variant(n)?).ok()
    }

    pub fn get_u32(&self, n: &str) -> Option<u32> {
        variant_u32(&self.variant(n)?).ok()
    }

    /// Validates a method result property. A missing property, or one that is present but
    /// NULL/EMPTY, means success (code 0);
    /// a present property that cannot be read as u32 is an error (never swallowed);
    /// a non-zero value becomes [`WmiError::Method`].
    fn check_return_value(
        &self,
        prop: &str,
        method: &str,
        object_path: &str,
    ) -> Result<(), WmiError> {
        let Some(v) = self.variant(prop).filter(|v| !is_null_or_empty(v)) else {
            return Ok(());
        };
        let code = variant_u32(&v)?;
        if code != 0 {
            return Err(WmiError::Method {
                code,
                message: format!("{method} on {object_path} returned {code}"),
            });
        }
        Ok(())
    }

    pub fn get_u16(&self, n: &str) -> Option<u16> {
        u16::try_from(&self.variant(n)?).ok()
    }

    pub fn get_str(&self, n: &str) -> Option<String> {
        let v = self.variant(n)?;
        BSTR::try_from(&v).ok().map(|b| b.to_string())
    }

    pub fn get_bool(&self, n: &str) -> Option<bool> {
        bool::try_from(&self.variant(n)?).ok()
    }

    /// The object's `__PATH`, usable as `object_path` for [`Wmi::call`].
    pub fn path(&self) -> Option<String> {
        self.get_str("__PATH")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_msft_disk_returns_objects() {
        let wmi = Wmi::connect(r"root\Microsoft\Windows\Storage").unwrap();
        let disks = wmi.query("SELECT Number FROM MSFT_Disk").unwrap();
        assert!(!disks.is_empty());
        assert!(disks[0].get_u32("Number").is_some());
    }

    #[test]
    fn missing_property_is_none_and_strings_work() {
        let wmi = Wmi::connect(r"root\cimv2").unwrap();
        let os = wmi
            .query("SELECT Caption, BuildNumber FROM Win32_OperatingSystem")
            .unwrap();
        assert_eq!(os.len(), 1);
        assert!(os[0].get_str("Caption").unwrap().contains("Windows"));
        assert!(os[0].get_u32("DoesNotExist").is_none());
        assert!(os[0].path().is_some());
    }

    #[test]
    fn failing_method_maps_to_error() {
        // Calling a nonexistent method must fail in ExecMethod itself (an HRESULT), which
        // surfaces as WmiError::Com. WmiError::Method is reserved for a non-zero
        // ReturnValue and is covered by `return_value_check_distinguishes_missing_unparseable_and_nonzero`.
        let wmi = Wmi::connect(r"root\default").unwrap();
        let r = wmi.call("StdRegProv", "NoSuchMethodWoA", &[]);
        let e = r.err().expect("expected error");
        assert!(matches!(e, WmiError::Com(_)), "{e:?}");
    }

    const HKLM: u32 = 0x8000_0002;

    #[test]
    fn call_with_inputs_returns_outputs() {
        // StdRegProv is a static method class readable without admin.
        let wmi = Wmi::connect(r"root\default").unwrap();
        let out = wmi
            .call(
                "StdRegProv",
                "GetStringValue",
                &[
                    ("hDefKey", Value::U32(HKLM)),
                    (
                        "sSubKeyName",
                        Value::Str(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion".into()),
                    ),
                    ("sValueName", Value::Str("ProductName".into())),
                ],
            )
            .unwrap();
        assert!(out.get_str("sValue").unwrap().contains("Windows"));
    }

    #[test]
    fn nonzero_return_value_is_method_error() {
        let wmi = Wmi::connect(r"root\default").unwrap();
        let err = wmi
            .call(
                "StdRegProv",
                "GetStringValue",
                &[
                    ("hDefKey", Value::U32(HKLM)),
                    ("sSubKeyName", Value::Str(r"SOFTWARE\NoSuchKeyWoA".into())),
                    ("sValueName", Value::Str("x".into())),
                ],
            )
            .err()
            .expect("expected error");
        assert!(matches!(err, WmiError::Method { code: 2, .. }), "{err:?}");
    }

    #[test]
    fn class_of_handles_path_shapes() {
        assert_eq!(class_of("MSFT_Disk.Number=1"), "MSFT_Disk");
        assert_eq!(class_of("MSFT_Disk"), "MSFT_Disk");
        assert_eq!(
            class_of(
                r#"\.\root\Microsoft\Windows\Storage:MSFT_Disk.ObjectId="{1}\.\PHYSICALDRIVE0""#
            ),
            "MSFT_Disk"
        );
    }

    #[test]
    fn get_u64_reads_real_uint64() {
        let wmi = Wmi::connect(r"root\Microsoft\Windows\Storage").unwrap();
        let disks = wmi.query("SELECT Size FROM MSFT_Disk").unwrap();
        assert!(!disks.is_empty());
        assert!(disks[0].get_u64("Size").unwrap() > 0);
        // Same value through a numeric-string property of another class.
        let os = Wmi::connect(r"root\cimv2")
            .unwrap()
            .query("SELECT TotalVisibleMemorySize FROM Win32_OperatingSystem")
            .unwrap();
        assert!(os[0].get_u64("TotalVisibleMemorySize").unwrap() > 0);
        assert!(disks[0].get_u64("DoesNotExist").is_none());
    }

    #[test]
    fn return_value_check_distinguishes_missing_unparseable_and_nonzero() {
        let wmi = Wmi::connect(r"root\cimv2").unwrap();
        let os = wmi
            .query("SELECT Caption, NumberOfUsers FROM Win32_OperatingSystem")
            .unwrap();
        let o = &os[0];
        // Missing property: success.
        assert!(o.check_return_value("DoesNotExist", "M", "P").is_ok());
        // Present but not readable as u32 (a Windows caption string): error, not success.
        let e = o.check_return_value("Caption", "M", "P").unwrap_err();
        assert!(matches!(e, WmiError::Com(_)), "{e:?}");
        // Present and non-zero, through the real `call` path: StdRegProv.GetStringValue for a
        // key that does not exist under HKLM returns ReturnValue = 2 (a missing value under an
        // existing key returns 1, so the missing-key shape is used for a stable code 2).
        let reg = Wmi::connect(r"root\default").unwrap();
        let e = reg
            .call(
                "StdRegProv",
                "GetStringValue",
                &[
                    ("hDefKey", Value::U32(HKLM)),
                    ("sSubKeyName", Value::Str(r"SOFTWARE\NoSuchKeyWoA".into())),
                    ("sValueName", Value::Str("NoSuchValueWoA".into())),
                ],
            )
            .err()
            .expect("expected error");
        assert!(matches!(e, WmiError::Method { code: 2, .. }), "{e:?}");
    }

    #[test]
    fn null_return_value_is_treated_as_missing() {
        // A freshly spawned input-parameters instance has every property NULL.
        let wmi = Wmi::connect(r"root\default").unwrap();
        let mut class_obj: Option<IWbemClassObject> = None;
        unsafe {
            wmi.svc
                .GetObject(
                    &BSTR::from("StdRegProv"),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None::<&IWbemContext>,
                    Some(&mut class_obj),
                    None,
                )
                .unwrap();
        }
        let mut in_sig: Option<IWbemClassObject> = None;
        unsafe {
            class_obj
                .unwrap()
                .GetMethod(
                    &HSTRING::from("GetStringValue"),
                    0,
                    &mut in_sig,
                    std::ptr::null_mut(),
                )
                .unwrap();
        }
        let inst = WmiObject {
            obj: unsafe { in_sig.unwrap().SpawnInstance(0).unwrap() },
        };
        let v = inst.variant("sValueName").expect("property exists");
        assert!(is_null_or_empty(&v), "expected a NULL property");
        assert!(inst.check_return_value("sValueName", "M", "P").is_ok());
    }
}
