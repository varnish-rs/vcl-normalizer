//! VMOD introspection: load JSON specs from .so binaries.
//!
//! Every vmod .so embeds a JSON spec between markers VMOD_JSON_SPEC\x02 and \x03.
//! This module extracts and parses that spec to validate vmod calls.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Complete VMOD specification extracted from a .so binary.
#[derive(Debug, Clone)]
pub struct VmodSpec {
    pub funcs: BTreeMap<String, Sig>,
    pub objects: BTreeMap<String, ObjSpec>,
}

/// Specification of a VMOD object (with init and methods).
#[derive(Debug, Clone)]
pub struct ObjSpec {
    pub init: Sig,
    pub methods: BTreeMap<String, Sig>,
}

/// Function signature: list of argument specifications.
#[derive(Debug, Clone)]
pub struct Sig {
    pub args: Vec<ArgSpec>,
}

/// Specification of a function/method argument.
#[derive(Debug, Clone)]
pub struct ArgSpec {
    pub name: Option<String>,    // None = positional-only
    pub default: Option<String>, // literal default value as string
    pub optional: bool,
    /// For an `ENUM`-typed argument: its legal literal values (e.g.
    /// `["IDENTITY", "BASE64", "HEX", ...]` for blob.encode's `encoding`
    /// arg), taken verbatim from the vmod's JSON spec. `None` for any
    /// non-ENUM argument.
    pub enum_values: Option<Vec<String>>,
}

const MARKER_START: &[u8] = b"VMOD_JSON_SPEC\x02";
const MARKER_END: u8 = b'\x03';

/// Get the default VMOD search paths from pkg-config.
/// Returns empty vec on failure (no warning emitted here; let the caller decide).
pub fn default_vmod_paths() -> Vec<PathBuf> {
    match std::process::Command::new("pkg-config")
        .arg("--variable=vmoddir")
        .arg("varnishapi")
        .output()
    {
        Ok(output) if output.status.success() => {
            let path_str = String::from_utf8_lossy(&output.stdout);
            vec![PathBuf::from(path_str.trim())]
        }
        _ => vec![],
    }
}

/// Load and parse a VMOD specification from a .so file.
///
/// For `import foo;`: searches for `libvmod_foo.so` in `vmod_paths` (in order).
/// For `import foo from "path";`: uses that path directly.
///
/// Returns `None` on any error (missing file, no marker, bad JSON, etc.),
/// and emits a warning to stderr.
pub fn load_vmod(name: &str, from: Option<&str>, vmod_paths: &[PathBuf]) -> Option<VmodSpec> {
    let so_path = if let Some(path) = from {
        PathBuf::from(path)
    } else {
        let so_name = format!("libvmod_{}.so", name);
        let mut found = None;
        for base_path in vmod_paths {
            let candidate = base_path.join(&so_name);
            if candidate.exists() {
                found = Some(candidate);
                break;
            }
        }
        found.unwrap_or_else(|| {
            eprintln!("warning: vmod '{}' not found in vmod paths", name);
            PathBuf::new()
        })
    };

    if so_path.as_os_str().is_empty() {
        return None;
    }

    // Read the file.
    let data = match std::fs::read(&so_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "warning: failed to read vmod '{}' from {:?}: {}",
                name, so_path, e
            );
            return None;
        }
    };

    // Find the marker.
    let start_idx = match find_bytes(&data, MARKER_START) {
        Some(idx) => idx + MARKER_START.len(),
        None => {
            eprintln!("warning: vmod '{}' has no VMOD_JSON_SPEC marker", name);
            return None;
        }
    };

    // Find the end marker.
    let end_idx = match data[start_idx..].iter().position(|&b| b == MARKER_END) {
        Some(pos) => start_idx + pos,
        None => {
            eprintln!(
                "warning: vmod '{}' has truncated VMOD_JSON_SPEC (no end marker)",
                name
            );
            return None;
        }
    };

    let json_bytes = &data[start_idx..end_idx];

    // Parse JSON.
    let spec_json: serde_json::Value = match serde_json::from_slice(json_bytes) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("warning: vmod '{}' has invalid JSON spec: {}", name, e);
            return None;
        }
    };

    // The spec is an array of arrays.
    let entries = match spec_json.as_array() {
        Some(arr) => arr,
        None => {
            eprintln!("warning: vmod '{}' spec is not a JSON array", name);
            return None;
        }
    };

    let mut funcs = BTreeMap::new();
    let mut objects = BTreeMap::new();

    for entry in entries {
        let entry_arr = match entry.as_array() {
            Some(arr) if !arr.is_empty() => arr,
            _ => continue,
        };

        match entry_arr[0].as_str() {
            Some("$VMOD") => {
                // Check version: entry_arr[1] should be a version string starting with "2"
                if let Some(version) = entry_arr.get(1).and_then(|v| v.as_str()) {
                    if !version.starts_with("2") {
                        eprintln!(
                            "warning: vmod '{}' has unsupported spec version '{}'",
                            name, version
                        );
                        return None;
                    }
                } else {
                    eprintln!("warning: vmod '{}' has no version in $VMOD entry", name);
                    return None;
                }
            }
            Some("$FUNC") => {
                // ["$FUNC", fname, signature]
                if let (Some(fname), Some(sig)) = (
                    entry_arr.get(1).and_then(|v| v.as_str()),
                    entry_arr.get(2).and_then(|v| v.as_array()),
                ) {
                    if let Some(spec) = parse_signature(sig) {
                        funcs.insert(fname.into(), spec);
                    }
                }
            }
            Some("$OBJ") => {
                // ["$OBJ", oname, {flags}, ctype, ["$INIT", signature], ["$FINI", …], ["$METHOD", mname, signature]*]
                if let Some(oname) = entry_arr.get(1).and_then(|v| v.as_str()) {
                    let mut init_sig = None;
                    let mut methods = BTreeMap::new();

                    for item in &entry_arr[2..] {
                        if let Some(item_arr) = item.as_array() {
                            if let Some(item_type) = item_arr.first().and_then(|v| v.as_str()) {
                                match item_type {
                                    "$INIT" => {
                                        if let Some(sig) =
                                            item_arr.get(1).and_then(|v| v.as_array())
                                        {
                                            if let Some(spec) = parse_signature(sig) {
                                                init_sig = Some(spec);
                                            }
                                        }
                                    }
                                    "$METHOD" => {
                                        // ["$METHOD", mname, signature]
                                        if let (Some(mname), Some(sig)) = (
                                            item_arr.get(1).and_then(|v| v.as_str()),
                                            item_arr.get(2).and_then(|v| v.as_array()),
                                        ) {
                                            if let Some(spec) = parse_signature(sig) {
                                                methods.insert(mname.into(), spec);
                                            }
                                        }
                                    }
                                    "$FINI" | "$CPROTO" | "$EVENT" => {
                                        // Ignored.
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    if let Some(init) = init_sig {
                        objects.insert(oname.into(), ObjSpec { init, methods });
                    }
                }
            }
            Some("$CPROTO") | Some("$EVENT") => {
                // Ignored.
            }
            _ => {
                // Unknown entry type, skip.
            }
        }
    }

    Some(VmodSpec { funcs, objects })
}

/// Parse a signature array: [[RETTYPE, …], cfunc_name, cproto_string, arg*]
fn parse_signature(sig: &[serde_json::Value]) -> Option<Sig> {
    // We only care about the args, which come at index 3 and beyond.
    let mut args = Vec::new();

    // Skip the first 3 elements (return type, cfunc name, cproto).
    for arg_val in &sig[3..] {
        if let Some(arg_arr) = arg_val.as_array() {
            if arg_arr.is_empty() {
                continue;
            }

            // arg := [TYPE, name|null, internal_name, default?, spec?, is_optional?]
            let name = arg_arr
                .get(1)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            // For an ENUM-typed arg, index 4 ("spec?") is the list of legal
            // literal values, e.g. ["IDENTITY", "BASE64", "HEX", ...].
            let enum_values = if arg_arr.first().and_then(|v| v.as_str()) == Some("ENUM") {
                arg_arr.get(4).and_then(|v| v.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
            } else {
                None
            };

            let default = arg_arr.get(3).map(|v| {
                // Default can be any JSON value; convert to string for storage.
                match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::Bool(b) => b.to_string(),
                    serde_json::Value::Null => "null".to_string(),
                    _ => v.to_string(),
                }
            });

            // An arg is optional if:
            // - It has a 4th element (default) (index 3)
            // - Or it has an explicit 6th element (is_optional) (index 5) that is true
            let optional = arg_arr.get(3).is_some()
                || arg_arr.get(5).and_then(|v| v.as_bool()).unwrap_or(false);

            args.push(ArgSpec {
                name,
                default,
                optional,
                enum_values,
            });
        }
    }

    Some(Sig { args })
}

/// Helper: find a byte sequence in a slice.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a test fixture .so file with embedded JSON spec.
    fn create_test_vmod(json_spec: &str) -> Vec<u8> {
        let mut data = Vec::new();
        // Arbitrary padding before marker.
        data.extend_from_slice(b"ELF\x7f\x00\x00\x00\x00padding");
        // The marker + JSON.
        data.extend_from_slice(MARKER_START);
        data.extend_from_slice(json_spec.as_bytes());
        data.push(MARKER_END);
        // Arbitrary padding after marker.
        data.extend_from_slice(b"more padding");
        data
    }

    #[test]
    fn v1_extract_spec_from_fixture() {
        // V1: Extract spec from fixture byte blob
        let spec_json = r#"[["$VMOD","2.0","foo"],["$FUNC","tolower",[["STRING"],null,"VCP_void",["STRING","s",null]]]]"#;
        let data = create_test_vmod(spec_json);

        // Write to a temp file and load.
        let temp_file = std::env::temp_dir().join("test_vmod_v1.so");
        std::fs::write(&temp_file, &data).unwrap();

        let spec = load_vmod("test_vmod_v1", Some(temp_file.to_str().unwrap()), &[]).unwrap();

        assert!(spec.funcs.contains_key("tolower"));
        let tolower_sig = &spec.funcs["tolower"];
        assert_eq!(tolower_sig.args.len(), 1);

        let _ = std::fs::remove_file(&temp_file);
    }

    #[test]
    fn v2_missing_marker_truncated_json_bad_version() {
        // V2: Missing marker / truncated JSON / $VMOD version "3.0" → warning + None
        let temp_dir = std::env::temp_dir();

        // Test 1: Missing marker
        let data = b"ELF\x7f\x00\x00padding only, no marker";
        let file1 = temp_dir.join("test_vmod_v2_no_marker.so");
        std::fs::write(&file1, data).unwrap();
        let result = load_vmod("test_vmod_v2_no_marker", Some(file1.to_str().unwrap()), &[]);
        assert!(result.is_none());

        // Test 2: Version "3.0" (unsupported)
        let spec_json = r#"[["$VMOD","3.0","foo"]]"#;
        let data = create_test_vmod(spec_json);
        let file2 = temp_dir.join("test_vmod_v2_bad_version.so");
        std::fs::write(&file2, &data).unwrap();
        let result = load_vmod(
            "test_vmod_v2_bad_version",
            Some(file2.to_str().unwrap()),
            &[],
        );
        assert!(result.is_none());

        // Test 3: Truncated JSON (no end marker)
        let mut data = Vec::new();
        data.extend_from_slice(b"ELF\x7f");
        data.extend_from_slice(MARKER_START);
        data.extend_from_slice(b"[incomplete json...");
        // No end marker
        let file3 = temp_dir.join("test_vmod_v2_truncated.so");
        std::fs::write(&file3, &data).unwrap();
        let result = load_vmod("test_vmod_v2_truncated", Some(file3.to_str().unwrap()), &[]);
        assert!(result.is_none());

        let _ = std::fs::remove_file(&file1);
        let _ = std::fs::remove_file(&file2);
        let _ = std::fs::remove_file(&file3);
    }

    #[test]
    fn v3_func_parsing_optional_args() {
        // V3: $FUNC parsing: names, arg count, optional-arg detection
        let spec_json = r#"[
            ["$VMOD","2.0","foo"],
            ["$FUNC","func_with_optional",[["STRING"],null,"VCP_void",
                ["STRING","required",null],
                ["STRING","optional_with_default","opt_internal","default_value"],
                ["INT",null,"positional_only_internal"]
            ]]
        ]"#;
        let data = create_test_vmod(spec_json);
        let temp_file = std::env::temp_dir().join("test_vmod_v3.so");
        std::fs::write(&temp_file, &data).unwrap();

        let spec = load_vmod("test_vmod_v3", Some(temp_file.to_str().unwrap()), &[]).unwrap();

        let func = &spec.funcs["func_with_optional"];
        assert_eq!(func.args.len(), 3);

        // First arg: required (name="required", no default)
        assert_eq!(func.args[0].name.as_deref(), Some("required"));
        assert_eq!(func.args[0].default, None);
        assert!(!func.args[0].optional);

        // Second arg: optional with default
        assert_eq!(func.args[1].name.as_deref(), Some("optional_with_default"));
        assert_eq!(func.args[1].default, Some("default_value".to_string()));
        assert!(func.args[1].optional);

        // Third arg: positional-only (name=null)
        assert_eq!(func.args[2].name, None);
        assert!(!func.args[2].optional);

        let _ = std::fs::remove_file(&temp_file);
    }

    #[test]
    fn v4_obj_parsing_init_and_methods() {
        // V4: $OBJ parsing: $INIT signature + $METHOD list; $CPROTO/$EVENT ignored
        let spec_json = r#"[
            ["$VMOD","2.0","foo"],
            ["$OBJ","myobj",{},"struct obj *",
                ["$INIT",[["VOID"],null,"myobj_init",["STRING","arg1",null]]],
                ["$CPROTO","..."],
                ["$EVENT","..."],
                ["$METHOD","method1",[["STRING"],null,"myobj_method1"]],
                ["$METHOD","method2",[["INT"],null,"myobj_method2",["STRING","param",null]]]
            ]
        ]"#;
        let data = create_test_vmod(spec_json);
        let temp_file = std::env::temp_dir().join("test_vmod_v4.so");
        std::fs::write(&temp_file, &data).unwrap();

        let spec = load_vmod("test_vmod_v4", Some(temp_file.to_str().unwrap()), &[]).unwrap();

        let obj = &spec.objects["myobj"];
        assert_eq!(obj.init.args.len(), 1);
        assert_eq!(obj.init.args[0].name.as_deref(), Some("arg1"));

        assert_eq!(obj.methods.len(), 2);
        assert!(obj.methods.contains_key("method1"));
        assert!(obj.methods.contains_key("method2"));
        assert_eq!(obj.methods["method2"].args.len(), 1);

        let _ = std::fs::remove_file(&temp_file);
    }

    #[test]
    fn v5_missing_so_file() {
        // V5: .so not found on vmod paths → warning, returns None
        let nonexistent_path = std::env::temp_dir().join("nonexistent_vmod_v5.so");
        let result = load_vmod("v5_missing", Some(nonexistent_path.to_str().unwrap()), &[]);
        assert!(result.is_none());
    }

    #[test]
    fn v6_system_vmod_if_available() {
        // V6: (integration, auto-skip) system libvmod_std.so via pkg-config
        // This test auto-skips if pkg-config is unavailable.
        let default_paths = default_vmod_paths();
        if default_paths.is_empty() {
            eprintln!("pkg-config not available; skipping V6 test");
            return;
        }

        // Try to load the system std vmod.
        if let Some(spec) = load_vmod("std", None, &default_paths) {
            // Should have tolower function (or similar, tolower is standard in vmod_std).
            assert!(!spec.funcs.is_empty(), "std vmod should contain functions");
        }
        // If loading fails, that's okay too (might not be installed).
    }
}
