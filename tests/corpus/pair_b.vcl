vcl 4.1;

# I3: hand-written pair (named top-level probe / positional vmod arg /
# "60s" duration). Functionally equivalent to pair_a.vcl (inline probe,
# named vmod arg, "1m" duration), same backend otherwise.

import fake from "tests/fixtures/fake_vmod.bin";

probe health_probe {
    .url = "/health";
    .interval = 5s;
    .timeout = 60s;
    .window = 5;
    .threshold = 3;
}

backend default {
    .host = "127.0.0.1";
    .port = "8080";
    .probe = health_probe;
}

sub vcl_recv {
    set req.http.X-Lower = fake.tolower(req.url);
    return (hash);
}
