vcl 4.1;

# I3: hand-written pair (inline probe / named vmod arg / "1m" duration).
# Paired with pair_b.vcl, which uses a named top-level probe, positional
# vmod args, and an equivalent "60s" duration. Written by hand (not by
# tools/mutate.py) to guard against the mutator and the comparator
# sharing blind spots.

import fake from "tests/fixtures/fake_vmod.bin";

backend default {
    .host = "127.0.0.1";
    .port = "8080";
    .probe = {
        .url = "/health";
        .interval = 5s;
        .timeout = 1m;
        .window = 5;
        .threshold = 3;
    }
}

sub vcl_recv {
    set req.http.X-Lower = fake.tolower(s = req.url);
    return (hash);
}
