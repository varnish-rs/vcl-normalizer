vcl 4.1;

# seed6: multiple same-named sub blocks — VCC concatenates them in
# declaration order, so vcl_recv here is really one subroutine made of
# three fragments.

backend default {
    .host = "127.0.0.1";
    .port = "8080";
}

sub vcl_recv {
    set req.http.X-Stage = "one";
}

sub vcl_recv {
    if (req.http.X-Stage == "one") {
        set req.http.X-Stage = "two";
    }
}

sub vcl_recv {
    set req.http.X-Stage = req.http.X-Stage + "-three";
    return (hash);
}
