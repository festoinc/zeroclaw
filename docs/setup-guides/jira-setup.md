# Jira Setup

ZeroClaw supports Jira Cloud via the REST API v3, allowing the agent to read
tickets, search with JQL, add comments, list projects, and verify credentials.

## Overview

| Action | Description | Mutating |
|--------|-------------|----------|
| `get_ticket` | Fetch a ticket by key with configurable detail level | No |
| `search_tickets` | Search issues with JQL | No |
| `list_projects` | List projects and their statuses/members | No |
| `myself` | Verify credentials and return account info | No |
| `comment_ticket` | Post a comment (supports @mentions and **bold**) | Yes |

## Setup

### 1. Get a Jira API Token

1. Go to [https://id.atlassian.com/manage-profile/security/api-tokens](https://id.atlassian.com/manage-profile/security/api-tokens)
2. Click **Create API token**
3. Give it a label (e.g. `zeroclaw`) and copy the token

### 2. Add credentials to `.env`

```bash
JIRA_BASE_URL=https://yourco.atlassian.net
JIRA_EMAIL=you@example.com
JIRA_API_TOKEN=your-jira-api-token
```

Never put credentials in `config.toml` — use `.env` only.

### 3. Enable Jira in `config.toml`

```toml
[jira]
enabled = true
allowed_actions = ["get_ticket", "search_tickets", "myself"]
timeout_secs = 30
```

`get_ticket` is the safe read-only default. Add other actions only as needed:
- `search_tickets` — JQL search, read-only
- `list_projects` — lists all projects and assignable users, read-only
- `myself` — verifies credentials, read-only
- `comment_ticket` — posts comments, **mutating** (requires Act policy)

## Verify Setup

### Test credentials directly

```bash
source .env
curl -s -u "$JIRA_EMAIL:$JIRA_API_TOKEN" \
  "$JIRA_BASE_URL/rest/api/3/myself" | python3 -m json.tool
```

Expected response includes your `accountId`, `displayName`, and `emailAddress`.

### Test via ZeroClaw agent

```bash
zeroclaw agent -m "Use the jira tool with action=myself to verify my credentials"
```

## Available Actions

### `get_ticket`

Fetches a single issue. Supports four detail levels via `level_of_details`:

| Level | Contents | Best for |
|-------|----------|----------|
| `basic` (default) | Summary, status, priority, assignee, rendered description, comments | Reading a ticket in full |
| `basic_search` | Lightweight fields only, no description or comments | Identifying a ticket |
| `full` | All Jira fields plus rendered HTML | Deep inspection (verbose) |
| `changelog` | Issue key and full change history | Audit trails |

### `search_tickets`

Searches issues using JQL. Returns up to 999 results (default: 25), paginated automatically.

Example JQL: `project = PROJ AND status = "In Progress" ORDER BY updated DESC`

### `comment_ticket`

Posts a comment in Atlassian Document Format (ADF). Supports:
- `@user@domain.com` — mention a user (resolved to account ID automatically)
- `**bold text**` — bold formatting
- `- item` — bullet list items
- Newlines become line breaks

Example:
```
Hi @john@company.com, this is **important**.
- Check the logs
- Rerun the pipeline
```

### `list_projects`

Returns all projects with their issue types, workflow statuses, and assignable users.

### `myself`

Returns your account ID, display name, email, and active status. Useful for verifying
that credentials are valid and the Jira connection is working.

## Troubleshooting

### `JIRA_BASE_URL env var must be set`

One of the three required env vars is missing. Check your `.env` file contains
`JIRA_BASE_URL`, `JIRA_EMAIL`, and `JIRA_API_TOKEN`, and that it is sourced
before starting ZeroClaw.

### `jira.allowed_actions contains unknown action`

The action name in `config.toml` is misspelled. Valid values:
`get_ticket`, `search_tickets`, `comment_ticket`, `list_projects`, `myself`.

### 401 Unauthorized

- Verify the API token has not expired
- Confirm the email matches the Atlassian account that owns the token
- Ensure `JIRA_BASE_URL` uses the `https://yourco.atlassian.net` format (no trailing slash)

### `comment_ticket` blocked in read-only mode

`comment_ticket` is a mutating action and requires the `Act` security policy.
Set `autonomy.level = "supervised"` or higher in `config.toml`.

## Related Documentation

- [ZeroClaw README](../../README.md)
- [Config Reference](../reference/config.md)
