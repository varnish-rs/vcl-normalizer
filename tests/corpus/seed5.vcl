vcl 4.1;

# seed5: long-strings + synthetic.
# NOTE: this seed intentionally does NOT exercise C{ ... }C inline C —
# varnishd forbids inline C unless started with -p vcc_feature=+backend_expression? no,
# with -p feature=+... actually the relevant flag is vcc_allow_inline_c, which is off
# by default. Inline-C ("C{ ... }C") coverage is unit-test-only (see lexer.rs L6 and
# parser tests); it is deliberately absent from the corpus so every seed here compiles
# with a stock `varnishd -C`.

backend default {
    .host = "127.0.0.1";
    .port = "8080";
}

sub vcl_recv {
    if (req.url ~ "^/blocked") {
        return (synth(403, "Forbidden"));
    }
    return (hash);
}

sub vcl_synth {
    set resp.http.Content-Type = "text/html; charset=utf-8";
    synthetic({"<html>
<head><title>"} + resp.status + " " + resp.reason + {"</title></head>
<body>
    <h1>"} + resp.status + " " + resp.reason + {"</h1>
    <p>This is a deliberately long synthetic body used to exercise the
    long-string form in this file, including embedded double quotes like
    these and multiple lines of text so the comparator has something
    substantial to normalize and print back out again without losing a
    single byte of the payload.</p>
</body>
</html>
"}
    );
    return (deliver);
}

sub vcl_deliver {
    set resp.http.X-Long-Note = """
        This header value uses the triple-quoted long-string form.
        It spans multiple lines and may contain "quoted" fragments
        without needing any escaping at all.
    """;
}
