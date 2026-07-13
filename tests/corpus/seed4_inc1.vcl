# seed4_inc1: backend + a sub called from the main recv path.

backend default {
    .host = "127.0.0.1";
    .port = "8080";
}

sub recv_from_inc1 {
    set req.http.X-Inc1 = "seen";
}
