vcl 4.1;

# seed2: typical mid-size VCL.
# 3 backends (one with an inline probe, one referencing a named probe,
# one plain), 2 ACLs, a couple of custom subs, and a round-robin
# director wired up in vcl_init.

import directors;

probe healthy_probe {
    .url = "/healthz";
    .interval = 5s;
    .timeout = 1s;
    .window = 5;
    .threshold = 3;
}

backend web01 {
    .host = "10.0.0.1";
    .port = "8080";
    .probe = {
        .url = "/status";
        .interval = 2s;
        .timeout = 500ms;
        .window = 3;
        .threshold = 2;
    }
}

backend web02 {
    .host = "10.0.0.2";
    .port = "8080";
    .probe = healthy_probe;
}

backend web03 {
    .host = "10.0.0.3";
    .port = "8080";
}

acl office_net {
    "192.168.1.0"/24;
    "10.0.0.0"/8;
    !"10.0.0.3";
}

acl purge_acl {
    "127.0.0.1";
    "::1";
}

sub vcl_init {
    new vdir = directors.round_robin();
    vdir.add_backend(web01);
    vdir.add_backend(web02);
    vdir.add_backend(web03);
}

sub check_purge_acl {
    if (client.ip !~ purge_acl) {
        return (synth(403, "Forbidden"));
    }
}

sub vcl_recv {
    set req.backend_hint = vdir.backend();

    if (req.method == "PURGE") {
        call check_purge_acl;
        return (purge);
    }

    if (client.ip ~ office_net) {
        set req.http.X-Internal = "true";
    }

    return (hash);
}

sub vcl_deliver {
    unset resp.http.X-Internal;
}
