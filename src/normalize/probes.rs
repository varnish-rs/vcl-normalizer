//! Pass 3: inline-probe lifting
//!
//! Lifts every FieldValue::Probe found in backend bodies into a new top-level
//! Decl::Probe named "$anon_probe_N" (N = 1-based encounter order),
//! replacing the field value with FieldValue::ProbeRef.

use crate::ast::*;

pub fn run(p: &mut Program) {
    let mut lifted_probes = Vec::new();
    let mut probe_counter = 1;

    // Walk through all backend declarations to find inline probes
    for decl in &mut p.decls {
        if let Decl::Backend {
            body: Some(fields), ..
        } = decl
        {
            for field in fields {
                // Check if this field contains an inline probe
                if let FieldValue::Probe(_) = field.value {
                    let probe_name = format!("$anon_probe_{}", probe_counter);
                    let span = field.span;

                    // Extract the probe fields by taking ownership via pattern matching
                    // We need to use a workaround since FieldValue doesn't implement Default
                    let probe_fields = if let FieldValue::Probe(pf) = std::mem::replace(
                        &mut field.value,
                        FieldValue::ProbeRef(probe_name.clone()),
                    ) {
                        pf
                    } else {
                        unreachable!()
                    };

                    // Create new Probe declaration
                    let new_probe = Decl::Probe {
                        name: probe_name.clone(),
                        body: probe_fields,
                        span,
                    };

                    lifted_probes.push(new_probe);
                    probe_counter += 1;
                }
            }
        }
    }

    // Append lifted probes to the program
    p.decls.extend(lifted_probes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    // Test N6: inline probe lifting
    #[test]
    fn test_single_inline_probe_lifted() {
        let inline_probe_fields = vec![
            builder::fexpr("url", builder::str_("/")),
            builder::fexpr("timeout", builder::dur(5.0)),
        ];

        let mut prog = builder::program(vec![builder::backend(
            "web",
            vec![
                builder::fexpr("host", builder::str_("example.com")),
                builder::field("probe", FieldValue::Probe(inline_probe_fields.clone())),
            ],
        )]);

        run(&mut prog);

        // Should have 2 declarations now: backend + probe
        assert_eq!(prog.decls.len(), 2);

        // Check backend still exists with ProbeRef
        if let Decl::Backend {
            body: Some(fields), ..
        } = &prog.decls[0]
        {
            assert_eq!(fields.len(), 2);
            assert!(
                matches!(&fields[1].value, FieldValue::ProbeRef(name) if name == "$anon_probe_1")
            );
        } else {
            panic!("Expected Backend declaration");
        }

        // Check lifted probe
        if let Decl::Probe { name, body, .. } = &prog.decls[1] {
            assert_eq!(name, "$anon_probe_1");
            assert_eq!(body.len(), 2);
            assert_eq!(body[0].name, "url");
            assert_eq!(body[1].name, "timeout");
        } else {
            panic!("Expected Probe declaration");
        }
    }

    #[test]
    fn test_two_backends_with_inline_probes() {
        let probe1_fields = vec![builder::fexpr("url", builder::str_("/health"))];
        let probe2_fields = vec![
            builder::fexpr("url", builder::str_("/status")),
            builder::fexpr("expected_response", builder::num("200")),
        ];

        let mut prog = builder::program(vec![
            builder::backend(
                "web1",
                vec![
                    builder::fexpr("host", builder::str_("web1.example.com")),
                    builder::field("probe", FieldValue::Probe(probe1_fields)),
                ],
            ),
            builder::backend(
                "web2",
                vec![
                    builder::fexpr("host", builder::str_("web2.example.com")),
                    builder::field("probe", FieldValue::Probe(probe2_fields)),
                ],
            ),
        ]);

        run(&mut prog);

        // Should have 4 declarations: 2 backends + 2 probes
        assert_eq!(prog.decls.len(), 4);

        // Check first backend's probe reference
        if let Decl::Backend {
            body: Some(fields), ..
        } = &prog.decls[0]
        {
            if let FieldValue::ProbeRef(name) = &fields[1].value {
                assert_eq!(name, "$anon_probe_1");
            } else {
                panic!("Expected ProbeRef in first backend");
            }
        }

        // Check second backend's probe reference
        if let Decl::Backend {
            body: Some(fields), ..
        } = &prog.decls[1]
        {
            if let FieldValue::ProbeRef(name) = &fields[1].value {
                assert_eq!(name, "$anon_probe_2");
            } else {
                panic!("Expected ProbeRef in second backend");
            }
        }

        // Check first lifted probe
        if let Decl::Probe { name, body, .. } = &prog.decls[2] {
            assert_eq!(name, "$anon_probe_1");
            assert_eq!(body.len(), 1);
        }

        // Check second lifted probe
        if let Decl::Probe { name, body, .. } = &prog.decls[3] {
            assert_eq!(name, "$anon_probe_2");
            assert_eq!(body.len(), 2);
        }
    }

    #[test]
    fn test_backend_without_inline_probe_unchanged() {
        let mut prog = builder::program(vec![builder::backend(
            "web",
            vec![
                builder::fexpr("host", builder::str_("example.com")),
                builder::field("probe", FieldValue::ProbeRef("my_probe".to_string())),
            ],
        )]);

        let original_len = prog.decls.len();
        run(&mut prog);

        // No new probes should be lifted
        assert_eq!(prog.decls.len(), original_len);

        // Backend should remain unchanged
        if let Decl::Backend {
            body: Some(fields), ..
        } = &prog.decls[0]
        {
            assert!(matches!(&fields[1].value, FieldValue::ProbeRef(name) if name == "my_probe"));
        }
    }

    #[test]
    fn test_probe_span_reused() {
        let inline_probe_fields = vec![builder::fexpr("url", builder::str_("/"))];

        let backend = builder::backend(
            "web",
            vec![builder::field(
                "probe",
                FieldValue::Probe(inline_probe_fields),
            )],
        );

        // Get the span of the probe field
        let field_span = if let Decl::Backend {
            body: Some(ref fields),
            ..
        } = backend
        {
            fields[0].span
        } else {
            panic!("Expected backend");
        };

        let mut prog = builder::program(vec![backend]);
        run(&mut prog);

        // Check that the lifted probe reuses the field's span
        if let Decl::Probe { span, .. } = &prog.decls[1] {
            assert_eq!(*span, field_span);
        }
    }
}
