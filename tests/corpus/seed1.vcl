vcl 4.1;

# seed1: minimal — one backend, vcl_recv only.

backend default {
    .host = "127.0.0.1";
    .port = "8080";
}

sub vcl_recv {
    if (req.method == "PURGE") {
        return (synth(405, "Not allowed"));
    }
    return (pass);
}
