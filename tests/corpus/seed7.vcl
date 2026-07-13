vcl 4.1;

# seed7: same-named (builtin) sub fragments interleaved with OTHER kinds
# of declarations (backend, probe, acl) between the fragments — not just
# other subs, as in seed6 — plus a plain custom (non-vcl_*) sub called
# from within one of the fragments.
#
# Note: only builtin `vcl_*` sub names may be legally redeclared like this
# (VCC's hook-chaining feature, confirmed against real `varnishd -C`); a
# custom sub name may only be declared once, so `log_request` below is
# NOT split into fragments.

sub vcl_recv {
    set req.http.X-Stage = "one";
}

backend default {
    .host = "127.0.0.1";
    .port = "8080";
}

sub vcl_recv {
    if (req.http.X-Stage == "one") {
        set req.http.X-Stage = "two";
    }
}

probe healthy_probe {
    .url = "/healthz";
    .interval = 5s;
    .timeout = 1s;
    .window = 5;
    .threshold = 3;
}

backend web02 {
    .host = "10.0.0.2";
    .port = "8080";
    .probe = healthy_probe;
}

sub log_request {
    set req.http.X-Log = "start-end";
}

acl trusted_net {
    "10.0.0.0"/8;
}

sub vcl_recv {
    call log_request;
    if (client.ip ~ trusted_net) {
        set req.http.X-Trusted = "true";
        set req.backend_hint = web02;
    }
    return (hash);
}
