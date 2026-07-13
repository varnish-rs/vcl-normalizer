vcl 4.1;

# seed4: multi-file — main + 2 includes.

include "seed4_inc1.vcl";
include "seed4_inc2.vcl";

sub vcl_recv {
    call recv_from_inc1;
    if (client.ip ~ trusted_net) {
        set req.http.X-Trusted = "true";
    }
    return (hash);
}
