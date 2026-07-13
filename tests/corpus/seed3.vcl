vcl 4.1;

# seed3: vmod-heavy — std and cookie, with a mix of positional and
# named/optional arguments.

import std;
import cookie;

backend default {
    .host = "127.0.0.1";
    .port = "8080";
}

sub vcl_recv {
    set req.http.X-Lower-Url = std.tolower(req.url);

    cookie.parse(req.http.cookie);
    cookie.keep("session,csrf");

    if (cookie.isset("session")) {
        set req.http.X-Session = cookie.get("session");
    }

    set req.http.X-Ttl = std.duration(s = "5s", fallback = 1s);
    set req.http.X-Sorted-Query = std.querysort(req.url);

    std.log("recv done for " + req.url);

    return (hash);
}

sub vcl_backend_response {
    set beresp.ttl = std.duration(s = "1m", fallback = 30s);
}
