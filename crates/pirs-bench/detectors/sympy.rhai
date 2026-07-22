// Sympy detector for the classic `bin/test` layout used by SWE-bench sympy
// instances. Without this, every sympy task fell through to agent-discovered
// self-report and often died at ReproFailed (fifty campaign: sympy-13647).
//
// Markers: `bin/test` + a top-level `sympy/` package. FAIL_TO_PASS ids are bare
// names like `test_col_insert` / `test_mod`.

fn detect() {
    let specs = [];
    if path_exists("bin/test") && path_exists("sympy") {
        let py = "PY=$(command -v python3 || command -v python); ";
        let b64 = "aW1wb3J0IHJlLHN5cyxzdWJwcm9jZXNzLHhtbC5ldHJlZS5FbGVtZW50VHJlZSBhcyBFVCxvcwppZHM9c3lzLmFyZ3ZbMTotMV0KanVuaXQ9c3lzLmFyZ3ZbLTFdCmlmIG5vdCBpZHM6CiAgICBvcGVuKGp1bml0LCJ3YiIpLndyaXRlKGInPHRlc3RzdWl0ZSBuYW1lPSJzeW1weSIvPicpCiAgICBzeXMuZXhpdCgwKQojIFByZWZlciBiaW4vdGVzdCAoc3ltcHkncyBydW5uZXIpOyBmYWxsIGJhY2sgdG8gcHl0aG9uIC1tIHB5dGVzdCBpZiBhYnNlbnQuCmJpbl90ZXN0PW9zLnBhdGguam9pbigiYmluIiwidGVzdCIpCmlmIG9zLnBhdGguaXNmaWxlKGJpbl90ZXN0KToKICAgIGNtZD1bc3lzLmV4ZWN1dGFibGUsIGJpbl90ZXN0LCAiLUMiLCAiLXEiXSArIGxpc3QoaWRzKQplbHNlOgogICAgY21kPVtzeXMuZXhlY3V0YWJsZSwgIi1tIiwgInB5dGVzdCIsICItcSJdICsgbGlzdChpZHMpCnA9c3VicHJvY2Vzcy5ydW4oY21kLHN0ZG91dD1zdWJwcm9jZXNzLlBJUEUsc3RkZXJyPXN1YnByb2Nlc3MuUElQRSx1bml2ZXJzYWxfbmV3bGluZXM9VHJ1ZSkKdGV4dD0ocC5zdGRvdXQgb3IgIiIpKyJcbiIrKHAuc3RkZXJyIG9yICIiKQpvdXRjb21lcz17fQojIHB5dGVzdCBub2RlIC8gYmFyZSBuYW1lczogInRlc3RfZm9vIFBBU1NFRCIgLyAidGVzdF9mb28gRkFJTEVEIiAvICJGQUlMRUQgdGVzdF9mb28iCmZvciBtIGluIHJlLmZpbmRpdGVyKHIiKFxTKnRlc3RbXHdcW1xdXC1cLl0rKVxzKyhQQVNTRUR8RkFJTEVEfEVSUk9SfFNLSVBQRUQpXGIiLCB0ZXh0LCByZS5JKToKICAgIG5hbWUsc3Q9bS5ncm91cCgxKSxtLmdyb3VwKDIpLmxvd2VyKCkKICAgIGxlYWY9bmFtZS5yc3BsaXQoIjo6IiwxKVstMV0ucnNwbGl0KCIuIiwxKVstMV0KICAgIG91dGNvbWVzW2xlYWZdPXN0CiAgICBvdXRjb21lc1tuYW1lXT1zdApmb3IgbSBpbiByZS5maW5kaXRlcihyIl4oUEFTU0VEfEZBSUxFRHxFUlJPUnxTS0lQUEVEKVxzKyhcUyp0ZXN0W1x3XFtcXVwtXC5dKykiLCB0ZXh0LCByZS5JfHJlLk0pOgogICAgc3QsbmFtZT1tLmdyb3VwKDEpLmxvd2VyKCksbS5ncm91cCgyKQogICAgbGVhZj1uYW1lLnJzcGxpdCgiOjoiLDEpWy0xXS5yc3BsaXQoIi4iLDEpWy0xXQogICAgb3V0Y29tZXNbbGVhZl09c3QKICAgIG91dGNvbWVzW25hbWVdPXN0CiMgc3ltcHkgYmluL3Rlc3QgY2xhc3NpYzogInRlc3RfZm9vIEUiIC8gInRlc3RfZm9vIEYiIC8gInRlc3RfZm9vIG9rIiBvbiBvd24gbGluZQpmb3IgbSBpbiByZS5maW5kaXRlcihyIl4oXFMqdGVzdF9cdyspXHMrKG9rfEZ8RXxmfGUpXHMqJCIsIHRleHQsIHJlLk0pOgogICAgbmFtZSxzdD1tLmdyb3VwKDEpLG0uZ3JvdXAoMikubG93ZXIoKQogICAgbGVhZj1uYW1lLnJzcGxpdCgiLiIsMSlbLTFdCiAgICBpZiBzdD09Im9rIjoKICAgICAgICBvdXRjb21lc1tsZWFmXT1vdXRjb21lc1tuYW1lXT0icGFzcyIKICAgIGVsaWYgc3QgaW4gKCJmIiwpOgogICAgICAgIG91dGNvbWVzW2xlYWZdPW91dGNvbWVzW25hbWVdPSJmYWlsIgogICAgZWxpZiBzdCBpbiAoImUiLCk6CiAgICAgICAgb3V0Y29tZXNbbGVhZl09b3V0Y29tZXNbbmFtZV09ImVycm9yIgojIFN1bW1hcnkgbGluZXMgZnJvbSBiaW4vdGVzdCAvIHB5dGVzdApmb3IgbSBpbiByZS5maW5kaXRlcihyIl4oRkFJTHxFUlJPUnxGQUlMRUQpXHMrKFxTKnRlc3RfXHcrKSIsIHRleHQsIHJlLkl8cmUuTSk6CiAgICBzdCxuYW1lPW0uZ3JvdXAoMSkubG93ZXIoKSxtLmdyb3VwKDIpCiAgICBsZWFmPW5hbWUucnNwbGl0KCIuIiwxKVstMV0KICAgIG91dGNvbWVzW2xlYWZdPW91dGNvbWVzW25hbWVdPSgiZXJyb3IiIGlmICJlcnJvciIgaW4gc3QgZWxzZSAiZmFpbCIpCnJvb3Q9RVQuRWxlbWVudCgidGVzdHN1aXRlIixuYW1lPSJzeW1weSIpCmZvciB0aWQgaW4gaWRzOgogICAgbGVhZj10aWQucnNwbGl0KCI6OiIsMSlbLTFdLnJzcGxpdCgiLiIsMSlbLTFdCiAgICBzdD1vdXRjb21lcy5nZXQodGlkKSBvciBvdXRjb21lcy5nZXQobGVhZikKICAgIHRjPUVULlN1YkVsZW1lbnQocm9vdCwidGVzdGNhc2UiLGNsYXNzbmFtZT0iIixuYW1lPWxlYWYpCiAgICBpZiBzdCBpcyBOb25lOgogICAgICAgIEVULlN1YkVsZW1lbnQodGMsInNraXBwZWQiKS50ZXh0PSJub3Qgc2VlbiBpbiBydW5uZXIgb3V0cHV0IGZvciAlcyIgJSB0aWQKICAgIGVsaWYgc3QgaW4gKCJwYXNzIiwicGFzc2VkIiwib2siKToKICAgICAgICBwYXNzCiAgICBlbGlmIHN0IGluICgiZmFpbCIsImZhaWxlZCIsImYiKToKICAgICAgICBFVC5TdWJFbGVtZW50KHRjLCJmYWlsdXJlIikudGV4dD10aWQKICAgIGVsaWYgc3QgaW4gKCJlcnJvciIsImUiKToKICAgICAgICBFVC5TdWJFbGVtZW50KHRjLCJlcnJvciIpLnRleHQ9dGlkCiAgICBlbHNlOgogICAgICAgIEVULlN1YkVsZW1lbnQodGMsInNraXBwZWQiKS50ZXh0PXRpZApFVC5FbGVtZW50VHJlZShyb290KS53cml0ZShqdW5pdCwgZW5jb2Rpbmc9InV0Zi04IiwgeG1sX2RlY2xhcmF0aW9uPVRydWUpCnN5cy5leGl0KDApCg==";
        specs.push(#{
            framework: "sympy",
            install: [],
            // Cheap confirmation: import sympy and prove bin/test exists.
            list_cmd: py + "test -f bin/test && \"$PY\" -c \"import sympy; print('sympy', sympy.__version__)\"",
            test_cmd: py + "echo " + b64 + " | base64 -d > /tmp/pirs_sympy_run.py && \"$PY\" /tmp/pirs_sympy_run.py {tests} {junit}",
            shell_quote_tests: true,
            timeout_secs: 1800,
            parallel: false,
        });
    }
    specs
}
