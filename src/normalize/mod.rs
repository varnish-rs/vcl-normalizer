pub mod literals;
pub mod probes;
pub mod rename;
pub mod sort;
pub mod vmod_args;

/// Runs the full normalize pipeline, in fixed order:
/// 1. literals — literal & symbol canonicalization
/// 2. vmod_args — vmod argument canonicalization (needs vmod specs)
/// 3. probes — inline-probe lifting
/// 4. rename — canonical renaming (returns the name bijection)
/// 5. sort — top-level declaration sort
///
/// When `rename` is `false`, pass 4's actual renaming is skipped and an
/// empty `NameMap` is returned -- but same-named *builtin* sub fragments
/// are still merged (`rename::merge_only`), since that reflects the
/// program VCC would actually run, not a naming-clarity choice. Used by
/// `vcl-normalizer print` by default: original backend/probe/acl/sub names are
/// clearer for a human than `backend_1`/`sub_2`/etc. `dump` and `compare`
/// always rename (`rename: true`) -- canonical names are required for
/// equivalence comparison.
pub fn normalize(
    p: &mut crate::ast::Program,
    specs: &std::collections::BTreeMap<String, crate::vmod::VmodSpec>,
    rename: bool,
) -> crate::ast::Result<crate::ast::NameMap> {
    literals::run(p);
    vmod_args::run(p, specs)?;
    probes::run(p);
    let names = if rename {
        crate::normalize::rename::run(p)
    } else {
        crate::normalize::rename::merge_only(p);
        crate::ast::NameMap::default()
    };
    sort::run(p);
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::builder::*;
    use crate::ast::{BinOp, FieldValue};
    use crate::canon;
    use crate::vmod::{ArgSpec, Sig, VmodSpec};
    use std::collections::BTreeMap;

    // N13: pass-order integration — full pipeline on one hand-built AST
    // exercising all five passes: a Num float literal (pass 1), a vmod call
    // with a default arg (pass 2), an inline probe (pass 3), a
    // shuffled/renameable backend (pass 4), and unsorted decls (pass 5).
    #[test]
    fn n13_full_pipeline_all_five_passes() {
        let fields_b = vec![
            fexpr("host", str_("b.example.com")),
            field(
                "probe",
                FieldValue::Probe(vec![
                    fexpr("url", str_("/health")),
                    // Pass 1: Num "2.50" -> canonical "2.5" via f64 round-trip.
                    fexpr("interval", num("2.50")),
                ]),
            ),
        ];
        let fields_a = vec![fexpr("host", str_("a.example.com"))];

        let vcl_recv_body = vec![
            // web_a is referenced first -> becomes backend_1 (pass 4 usage-order).
            set(&["req", "http", "x"], var(&["web_a"])),
            set(&["req", "http", "y"], var(&["web_b"])),
            if_(
                vec![(
                    bin(BinOp::Match, var(&["client", "ip"]), var(&["secure"])),
                    vec![],
                )],
                None,
            ),
            // Pass 2: resolve=false equals the spec default -> Omitted -> truncated.
            expr_stmt(fcall(
                &["std", "log"],
                vec![arg(str_("hi")), narg("resolve", bool_(false))],
            )),
        ];

        // Declared in shuffled / unsorted order on purpose.
        let mut prog = program(vec![
            sub("vcl_recv", vcl_recv_body),
            backend("web_b", fields_b),
            backend("web_a", fields_a),
            acl("secure", vec![acl_entry("10.0.0.0", None, false)]),
        ]);

        let mut funcs = BTreeMap::new();
        funcs.insert(
            "log".to_string(),
            Sig {
                args: vec![
                    ArgSpec {
                        name: Some("msg".to_string()),
                        default: None,
                        optional: false,
                        enum_values: None,
                    },
                    ArgSpec {
                        name: Some("resolve".to_string()),
                        default: Some("false".to_string()),
                        optional: true,
                        enum_values: None,
                    },
                ],
            },
        );
        let mut specs = BTreeMap::new();
        specs.insert(
            "std".to_string(),
            VmodSpec {
                funcs,
                objects: BTreeMap::new(),
            },
        );

        let names = normalize(&mut prog, &specs, true).expect("normalize should succeed");

        // Pass 4 bijection: web_a used first -> backend_1, web_b second -> backend_2.
        let backend_name = |orig: &str| -> String {
            names
                .entries
                .iter()
                .find(|e| e.0 == "backend" && e.2 == orig)
                .unwrap_or_else(|| panic!("no rename entry for {orig}"))
                .1
                .clone()
        };
        assert_eq!(backend_name("web_a"), "backend_1");
        assert_eq!(backend_name("web_b"), "backend_2");

        let json = canon::to_json(&prog);
        let decls = json["decls"].as_array().unwrap();

        // Pass 5: sorted by kind_rank (probe < backend < acl < sub).
        let kinds: Vec<&str> = decls.iter().map(|d| d["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["probe", "backend", "backend", "acl", "sub"]);

        // Pass 3: inline probe lifted to top-level, renamed to probe_1 (pass 4).
        let probe = &decls[0];
        assert_eq!(probe["name"], "probe_1");
        let interval_field = probe["body"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "interval")
            .unwrap();
        assert_eq!(interval_field["value"]["value"]["value"], "2.5");

        // backend_2 (former web_b) still references the lifted, renamed probe.
        let b2 = decls
            .iter()
            .find(|d| d["type"] == "backend" && d["name"] == "backend_2")
            .unwrap();
        let probe_field = b2["body"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "probe")
            .unwrap();
        assert_eq!(probe_field["value"]["type"], "probe_ref");
        assert_eq!(probe_field["value"]["value"], "probe_1");

        // Pass 2: default-valued optional arg collapsed to Omitted and truncated.
        let vcl_recv = decls.iter().find(|d| d["name"] == "vcl_recv").unwrap();
        let body = vcl_recv["body"].as_array().unwrap();
        let log_stmt = &body[3];
        assert_eq!(log_stmt["type"], "expr");
        let call_expr = &log_stmt["expr"];
        assert_eq!(call_expr["type"], "call");
        let call_args = call_expr["value"]["args"].as_array().unwrap();
        assert_eq!(call_args.len(), 1);
        assert_eq!(call_args[0]["name"], serde_json::Value::Null);
        assert_eq!(call_args[0]["value"]["type"], "str");
        assert_eq!(call_args[0]["value"]["value"], "hi");
    }
}
