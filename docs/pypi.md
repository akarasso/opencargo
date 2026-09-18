# PyPI repositories

Hosted, proxy and group repositories speak the Simple Repository API (PEP 503 HTML and PEP 691 JSON, API 1.1) and the legacy upload API.

| route | purpose |
|---|---|
| `GET /{repo}/simple/` | project index (hosted members only; an upstream index is never enumerated) |
| `GET /{repo}/simple/{project}/` | project page; a non-normalized name redirects to its PEP 503 spelling |
| `GET /{repo}/files/{project}/{filename}` | a file, and `{filename}.metadata` for a wheel's core metadata (PEP 658/714) |
| `POST /{repo}/legacy/` | upload (twine, poetry) |
| `POST` / `DELETE /{repo}/pypi/{project}/{version}/yank` | yank (optional JSON body `{"reason": ".."}`) / unyank |
| `DELETE /{repo}/pypi/{project}/{version}` | delete a release |
| `DELETE /{repo}/pypi/{project}` | delete every release of a project |

Credentials are `__token__` as the Basic username and an API token as the password; PyPI routes answer a 401 with a Basic challenge.

```sh
twine upload --repository-url https://registry.example/py-local/legacy/ -u __token__ -p "$TOKEN" dist/*
pip install --index-url "https://__token__:$TOKEN@registry.example/pypi-all/simple/" demo
uv pip install --index-url "https://__token__:$TOKEN@registry.example/pypi-all/simple/" demo
```

A proxy's `upstream` is the index base, e.g. `https://pypi.org/simple`. File URLs on its pages are fetched only from allowed hosts: `files.pythonhosted.org` and the index's own endpoint by default, or the repository's `file_hosts` list (`host` or `host:port`). Upstream credentials are sent only to the index's endpoint and never follow a redirect to another host. A file is verified against the sha256 its page announced before it is served.

In a group, the first member whose page lists a filename serves it, whatever the outcome; a member whose page fails or does not list the name passes to the next.

Twine 6.1 and later refuse `--skip-existing` for any index but PyPI's; re-uploading identical bytes is accepted without it, and different bytes under a published filename answer 409.
