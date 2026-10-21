# Poll through the forge CLIs, not over HTTP

babysit makes every forge call by running `gh api` or `glab api` instead of calling the forges' APIs
with its own HTTP client. The usual reason to go direct, polls made free by ETags and `304`s,
doesn't apply here: GitHub's thread resolution and review decision exist only in GraphQL, which
gets no `304`s, and GitLab counts a `304` against the rate limit anyway. The CLIs also own sign-in,
including refreshing GitLab's two-hour OAuth tokens, which a borrowed token would let go stale.
babysit already needs them to resolve checkout references and to tell which forge a host belongs to,
and the choice keeps an HTTP client, an async runtime, and a TLS stack out of the build. See
[Does babysit poll through gh/glab or over direct HTTP?](https://github.com/brzzdev/babysit/issues/9).

## Consequences

A poll costs a process spawn. Errors come back as an exit status and stderr text, not typed HTTP
responses, and `-i` is needed to read rate-limit headers.
