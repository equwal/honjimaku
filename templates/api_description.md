### Overview

Welcome to the API of this site. You can use this API to access the contents of the site. The API is based off of a simple REST API with a few endpoints.

### Authentication

The site uses API keys to allow access to the API. Authentication is done using the `Authorization` header. Note that in order to use this API, an account is required. Please [register](/login) if you have not done so already.

If you have not generated an API key yet, you can do so on your [account page](/account).

### Core Concepts

The site is basically a directory listing where every [Entry](#model/entry) represents a directory. On a site for anime these directories are backed by either a TMDB ID or an AniList ID. On a site for books they are backed by the `book_id` of the audiobook: an Audible ASIN, which the site verifies against the Audible catalog. Users with editor privileges can bypass this requirement for extraordinary cases.

Each directory entry has a set of files that can be downloaded.

On a site for books an entry also has a `kind` (`book`, `anime`, or `drama` for a live action show) and a `language` (an ISO 639-1 code). The anime and the live action shows are a live copy of another jimaku site. The search takes `kind` and `language` to narrow the results.

#### TMDB ID

A TMDB ID is encoded in string form in either `tv:id` or `movie:id` form (for example, `tv:1234`). In the future this syntax might be extended to support seasons in a TV show.

### Rate Limits

Rate limits are enforced at an IP level to prevent abuse and spam on the service. When a rate limit is hit, an HTTP 429 status code is returned with some header information telling you how to proceed.

#### Header Format

The following headers are returned when using a rate limited endpoint:

```
x-ratelimit-limit: 25
x-ratelimit-remaining: 14
x-ratelimit-reset: 1713373688
x-ratelimit-reset-after: 0.98
```
- **x-ratelimit-limit**: The number of requests that can be made.
- **x-ratelimit-remaining**: How many requests are left before hitting a 429.
- **x-ratelimit-reset**: The UNIX timestamp (seconds since midnight UTC on January 1st 1970) at which the rate limit resets. This can have a fractional component for milliseconds.
- **x-ratelimit-reset-after**: The total time in seconds to wait for the rate limit to restart. This can have a fractional component for milliseconds.

### Support

If you need any more help, or if you want to report a bug or request a feature, please don't hesitate to [contact us](/contact).
