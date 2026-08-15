# AGENTS.md

## Rust guidelines

### Design patterns & conventions

- Generally avoid clones, but pay special attention when cloning in a high-frequency code path
  - In such cases, reach for smart pointers (e.g., `Arc<T>`) instead
- Implement `From`/`TryFrom` for performing conversion between types
- Prefer the actor pattern for async tasks
  - Model as a struct encapsulating local state with an async, consuming run method
  - Other methods can operate on `&self` to keep `run` small

### Safety

- Avoid `unwrap` except in tests
  - When unavoidable, prefer `expect` instead and provide a concise message explaining what went wrong (e.g., "Invalid state")
- Avoid `unsafe` unless absolutely necessary
- When unavoidable, follow these guidelines
  - Wrap unsafe code in a safe function or struct
  - Isolate only the unsafe operations
  - Every unsafe block should have a `// SAFETY:` comment explaining why the operation is actually safe (e.g., verifying pointers are non-null)

### Style

- Only add comments when doing so genuinely points out something non-obvious
  - Brevity is always a must
- Avoid excessive nesting and prefer [`let-else`](https://doc.rust-lang.org/rust-by-example/flow_control/let_else.html)
- Function arguments
  - Generally, functions should only accept one argument
  - To accept more than one argument, define a struct with named field and accept it as the only argument
  - Two positional arguments are acceptable only when it is obvious what they represent (e.g., `Vec::new(1, 2)`, reader knows these represent x, y)

## Release process

- There is currently no automatic release process in place
- To create a release
  - Use `scripts/update_version.sh` to bump the version and update lock files
  - Open a PR (e.g, "Release v0.1.0") with the bumped version
  - Publish GitHub release with tag (e.g., v0.1.0)
    - Include automatically generated changelog
  - When the release is published and the tag is created, new release will be pushed to PyPi
- Eventually this will be automated
