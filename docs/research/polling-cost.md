# Polling cost on GitHub and GitLab

Research for [#3](https://github.com/brzzdev/babysit/issues/3). Checked 2026-10-20 against
`gh` 2.102.0 and `glab` 1.123.0. **Documented** facts cite the forge or CLI docs.
**Observed** facts come from read-only requests against public projects (`cli/cli` PR #14751 on
GitHub, `gitlab-org/cli` MR !4149 on GitLab) made while logged in.

## Question

What are each forge's rate limits for REST and GraphQL? Do conditional requests (`If-None-Match`,
`If-Modified-Since`) get a 304, and does a 304 count against the limit? Which of the endpoints a
watcher needs support them? How do `gh api` and `glab api` expose status and headers? How can a
direct HTTP client reuse the token `gh` or `glab` already holds, self-hosted hosts included?

## Answer in brief

- **GitHub REST:** an authenticated conditional GET that returns 304 is free against the primary
  limit. Every endpoint a PR watcher needs returns an ETag and honours it. Polling unchanged data
  costs nothing from the 5,000/h budget.
- **GitHub GraphQL:** a POST, so no conditional requests. One PR snapshot query costs 1 point of
  the 5,000/h budget per poll, whether anything changed or not.
- **GitLab REST:** returns weak ETags and answers 304, **but every 304 counts against the rate
  limit** (observed). A 304 saves bandwidth and nothing else. Budget per request.
- **`gh api -i` and `glab api -i` both exit 1 on a 304**, print the status line and headers on
  stdout, and write `gh: HTTP 304` / `glab: HTTP 304` on stderr. A wrapper has to read the status
  line, because the exit code alone looks the same as a 404 or 500.
- **Borrowing tokens:** `gh auth token [--hostname H]` prints the token. `glab` has no
  `auth token` command, but `glab config get token --host H` prints the stored token, keyring
  included (observed). GitLab OAuth tokens expire after two hours, so a borrowed `glab` token
  can go stale.

## Findings

### GitHub: rate limits

Documented in
[Rate limits for the REST API](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api):

- Primary: 5,000 requests/h for an authenticated user, 60/h unauthenticated, 1,000/h per repo for
  `GITHUB_TOKEN`, and 5,000 to 12,500/h for GitHub App installations.
- Secondary: at most 100 concurrent requests, 900 points/min for REST, and 90 s of CPU per 60 s.
  Most `GET`/`HEAD`/`OPTIONS` requests cost 1 point and most mutating requests cost 5. Some
  endpoints have point costs GitHub does not publish.
- Headers: `x-ratelimit-limit`, `-remaining`, `-used`, `-reset` (epoch seconds), and
  `-resource`. The headers are authoritative over `GET /rate_limit`.
- Exceeding a limit gets a 403 or 429. For the primary limit, wait until `x-ratelimit-reset`.
  For the secondary limit, honour `retry-after`. If it is absent and remaining is 0, wait until
  the reset time. Otherwise wait at least a minute, backing off exponentially. Retrying while
  limited can get an integration banned.

Documented in
[Rate limits and query limits for the GraphQL API](https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api):
5,000 points/h per user, a minimum of 1 point per call, and a secondary limit of 2,000
points/min. `rateLimit { cost remaining used resetAt }` reports the cost. GraphQL and REST have
separate budgets: `x-ratelimit-resource` is `core` for REST and `graphql` for GraphQL.

**Observed:** a query fetching a PR's `reviewDecision`, its last 50 comments and reviews, and
the head commit's `statusCheckRollup` with 100 contexts cost **1 point**.

### GitHub: conditional requests

Documented in
[Best practices for using the REST API](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api#use-conditional-requests-if-appropriate):

- "Most endpoints return an `etag` header, and many endpoints return a `last-modified` header."
  Send the stored value back as `if-none-match` or `if-modified-since`.
- "Making a conditional request does not count against your primary rate limit if a `304`
  response is returned **and the request was made while correctly authorized with an
  `Authorization` header**."
- Unsafe methods (`POST`, `PUT`, `PATCH`, `DELETE`) are not supported. That rules out GraphQL,
  which is always a POST.
- "Use the same parameters every time you poll the same data." A different page size, page
  number or filter produces a different ETag.
- If a response carries `x-poll-interval`, wait at least that many seconds. Make requests
  serially, not concurrently, to stay clear of secondary limits.
- Neither page says whether a 304 counts against the **secondary** limit.

**Observed:** each endpoint was fetched twice with 200s, then once with the ETag in
`If-None-Match`. Every endpoint returned `304 Not Modified`, and `X-Ratelimit-Used` rose only by
the two 200s, never by the 304:

| Endpoint | ETag | `Last-Modified` | 304 free |
|---|---|---|---|
| `GET /repos/{o}/{r}/pulls/{n}` | strong | yes | yes |
| `GET /repos/{o}/{r}/pulls/{n}/reviews` | weak | no | yes |
| `GET /repos/{o}/{r}/pulls/{n}/comments` (review comments) | strong | no | yes |
| `GET /repos/{o}/{r}/issues/{n}/comments` | weak | no | yes |
| `GET /repos/{o}/{r}/issues/{n}/timeline` | weak | no | yes |
| `GET /repos/{o}/{r}/issues/{n}/events` | weak | no | yes |
| `GET /repos/{o}/{r}/commits/{sha}/check-runs` | weak | no | yes |
| `GET /repos/{o}/{r}/commits/{sha}/status` | weak | no | yes |
| `GET /repos/{o}/{r}/actions/runs?head_sha={sha}` | weak | no | yes |
| `GET /notifications` | weak | yes | yes (sends `X-Poll-Interval: 60`) |

Also observed:

- One 200 returned a weak `W/"…"` ETag, and the 304 for it echoed the same value without the
  `W/`. Sending the weak form back worked. Store whatever the last 200 sent and send it verbatim.
- The 200 responses carry `Cache-Control: private, max-age=60, s-maxage=60` and
  `Vary: Accept, Authorization, …`.
- The PR was closed, so nothing changed between requests. On a live PR, a 304 depends on the
  representation staying byte-stable: timestamps such as check-run durations will change the
  ETag while CI runs.

### GitLab: rate limits

Documented in [GitLab.com rate limits](https://docs.gitlab.com/user/gitlab_com/rate_limits/):

- In force today: 2,000 requests/min per user for authenticated API traffic, and 500/min per IP
  unauthenticated.
- Proposed per-plan limits, not yet in force: Free 5,000/h and 100/min, Premium 15,000/h and
  1,250/min, Ultimate 25,000/h and 2,000/min, with the hourly limit taking precedence. Under Free,
  60/h unauthenticated.
- Endpoint-specific limits exist. Note creation is limited to 60/min, a single project to
  400/min, and pipeline creation to 25/min. No read limit is listed for MR notes, discussions or
  pipelines.
- "All responses, throttled or not, include `RateLimit-Limit`, `RateLimit-Remaining`, and related
  headers." The Projects, Groups and Users APIs are the exception. On a 429, wait for
  `Retry-After`. `RateLimit-ResetTime` gives the reset as a date.
- Self-managed instances set their own limits
  ([Rate limits](https://docs.gitlab.com/security/rate_limits/)). babysit cannot assume
  GitLab.com's numbers there.

**Observed:** API responses carried `ratelimit-name: throttle_authenticated_api`,
`ratelimit-limit: 2000`, `ratelimit-observed`, `ratelimit-remaining` and `ratelimit-reset`. The
reset was about a minute out, which is consistent with the per-minute limit in force today.

### GitLab: conditional requests

The REST API docs ([REST API](https://docs.gitlab.com/api/rest/)) say nothing about ETags or 304.
The only ETag document is the developer guide
[Polling with ETag caching](https://docs.gitlab.com/development/polling/). It describes a
Redis-backed ETag that answers 304 "without querying the database at all". That mechanism covers
only paths registered in `Gitlab::EtagCaching::Router`, and query parameters are not allowed on
them. The registered paths are internal web-UI JSON routes, such as
`…/merge_requests/:iid/pipelines.json`, `…/merge_requests/:iid/cached_widget.json` and
`…/pipelines/:id.json` ([router source](https://gitlab.com/gitlab-org/gitlab/-/blob/master/lib/gitlab/etag_caching/router/rails.rb)),
plus a few GraphQL resources selected by an `X-GITLAB-GRAPHQL-RESOURCE-ETAG` header
([graphql router](https://gitlab.com/gitlab-org/gitlab/-/blob/master/lib/gitlab/etag_caching/router/graphql.rb)).
None of them is under `/api/v4`.

**Observed** on `/api/v4`:

- Every endpoint tried returned a weak ETag (`W/"<32 hex>"`, an MD5-sized digest) and answered
  `If-None-Match` with `304 Not Modified`. The endpoints were MR detail, MR notes
  (`?sort=asc&order_by=updated_at`), MR discussions, MR pipelines, MR approvals, commit statuses
  and issue notes.
- **The 304 decremented `RateLimit-Remaining` by one, the same as a 200.** The claim "GitLab
  answers 304 but still counts it" holds.
- Responses carry `Cache-Control: max-age=0, private, must-revalidate`, and the 304 carried
  `Cf-Cache-Status: BYPASS`.
- Inference, not documented: a body-hash ETag on a Rails app suggests Rack's generic
  `ETag`/`ConditionalGet` middleware. That middleware renders the full response before comparing,
  so the server does the same work for a 304. A 304 saves transfer and JSON parsing on our side.

### `gh api` and `glab api`: status and headers

Documented in [`gh api`](https://cli.github.com/manual/gh_api):
`-i/--include` "Include HTTP response status line and headers in the output",
`-H/--header key:value`, `--hostname`, `--silent`, `--jq`, `--paginate`, and `--cache <duration>`.
The exit codes in [`gh help exit-codes`](https://cli.github.com/manual/gh_help_exit-codes) are 0
for success, 1 for any failure, 2 for cancelled and 4 for auth required. The manual says nothing
specific about HTTP status or 304.

Documented in `glab api --help`, the CLI's own help as of 1.123.0: `-i/--include` "Include HTTP
response headers in the output", `-H/--header`, `--hostname`, `--silent` and `--paginate`.
`GLAB_DEBUG_HTTP=1` dumps the request and response.

**Observed:**

- `gh api -i -H 'If-None-Match: …'` printed `HTTP/2.0 304 Not Modified` and headers on stdout,
  wrote `gh: HTTP 304` to stderr, and **exited 1**. The claim holds.
- `glab api -i -H 'If-None-Match: …'` did the same: `HTTP/2.0 304 Not Modified` on stdout,
  `glab: HTTP 304` on stderr, exit 1.
- So a subprocess transport has to parse the first line of `-i` output to tell "unchanged" from
  a real error, and pay a process spawn per request. A direct HTTP client gets the status code
  directly.

### Borrowing the CLI's token

GitHub:

- [`gh auth token`](https://cli.github.com/manual/gh_auth_token) "outputs the authentication token
  for an account on a given GitHub host", with `-h/--hostname` and `-u/--user`.
- Per [`gh help environment`](https://cli.github.com/manual/gh_help_environment), `GH_TOKEN` then
  `GITHUB_TOKEN` override stored credentials for github.com and `*.ghe.com`. `GH_ENTERPRISE_TOKEN`
  then `GITHUB_ENTERPRISE_TOKEN` do the same for GitHub Enterprise Server hosts. `GH_HOST` sets
  the default host, and `GH_CONFIG_DIR` the config dir (default `~/.config/gh`).
- Send the token as `Authorization: Bearer <token>`. The free 304 requires an `Authorization`
  header.

GitLab:

- Token precedence ([Authenticate with GitLab](https://docs.gitlab.com/cli/authentication/)):
  `GITLAB_TOKEN`, then `GITLAB_ACCESS_TOKEN`, then `OAUTH_TOKEN`. After those come the credentials
  stored for the host, in the OS keyring or the config file, and then a CI job token if CI
  auto-login is on. The instance is chosen by `GITLAB_HOST` or `--hostname`, and inside a repo it
  is inferred from git remotes.
- `glab auth login` stores credentials in the OS keyring by default (macOS Keychain, Windows
  Credential Manager, Linux Secret Service). It falls back to plaintext in `config.yml` with no
  keyring, under `--insecure-storage`, or in CI.
- Config search order ([Configure the CLI](https://docs.gitlab.com/cli/configuration/)):
  `$GLAB_CONFIG_DIR/config.yml`, then legacy `~/.config/glab-cli/config.yml`, then
  `$XDG_CONFIG_HOME/glab-cli/config.yml`, then `$XDG_CONFIG_DIRS`. On macOS the XDG location is
  `~/Library/Application Support/glab-cli/config.yml`. `glab config path` prints the file in use.
  Hosts are keyed per host, and `token` is "Scoped per host; set it with `--host`".
- No `glab auth token` command exists. `glab config get token` is not in the auth docs, but
  `glab config --help` lists `token` as a per-host key.
  **Observed:** `glab config get token --host gitlab.com` printed the token kept in the keyring
  (exit 0, 64 characters, an OAuth token). It authenticated `GET /api/v4/user` as
  `Authorization: Bearer`.
- Header formats ([REST API authentication](https://docs.gitlab.com/api/rest/authentication/)):
  `PRIVATE-TOKEN: <pat>` for access tokens, and `Authorization: Bearer <token>` for OAuth tokens
  and, equally, for personal, project or group access tokens.
- OAuth access tokens are issued with `"expires_in": 7200`, two hours
  ([OAuth 2.0 API](https://docs.gitlab.com/api/oauth2/)). Refreshing invalidates the old pair.
  `glab` refreshes on use, so a token read with `glab config get token` may be expired, and
  babysit must not refresh it itself, or it will invalidate the refresh token `glab` holds.

### What this means for a watcher's budget

Take one PR, five REST endpoints, a poll every 30 s and a restart every 10 minutes:

- **GitHub REST with ETags:** about 600 requests/h, but only the 200s count. Unchanged polls are
  free. If ETags are lost on restart, that costs at most 5 × 6 = 30 requests/h, so persisting
  ETags is optional.
- **GitHub GraphQL:** 120 points/h for one target with no 304 discount, and it covers everything
  in one round trip.
- **GitLab REST:** about 600 counted requests/h per target, 304 or not. That is trivial under
  today's 2,000/min. Under the proposed Free limit of 5,000/h it allows about 8 targets at that
  rate, and the 100/min burst limit also matters if polls fan out at once. Fewer endpoints per
  poll, such as one `notes?order_by=updated_at` plus `pipelines`, matter more than ETags here.

## Verify manually

1. **Secondary limits and 304s on GitHub.** The docs exempt 304s only from the *primary* limit.
   Whether they cost secondary-limit points is undocumented.
2. **GitHub ETag stability on a live PR.** Does `check-runs` or `actions/runs` change its ETag on
   every poll while CI is running (durations, `updated_at`), so that 304s rarely happen while CI
   is in progress?
3. **GitLab's proposed per-plan limits.** They are "not yet in effect". Check
   [GitLab.com rate limits](https://docs.gitlab.com/user/gitlab_com/rate_limits/) before fixing a
   default poll interval or target cap.
4. **Self-managed GitLab.** Whether a 304 counts there depends on the admin's Rack::Attack
   settings. It was not tested.
5. **`glab config get token` as an interface.** It is undocumented as a token-retrieval path.
   Also unverified: whether it returns an expired OAuth token or refreshes first, and how often
   babysit would hit an expired token. A cheap `glab api user` before reading the token may be
   needed to force a refresh.
6. **`gh api --cache`.** It is undocumented whether the CLI's response cache revalidates with
   `If-None-Match` or just serves stale data. Do not rely on it for polling.
7. **GitHub `x-poll-interval`.** It was seen on `/notifications` only. Check whether any
   PR/issue endpoint ever sends it.
