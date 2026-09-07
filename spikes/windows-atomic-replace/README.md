# Windows atomic replacement

This executable checks that SUSM can replace a synced runtime journal or compressed log segment while a reader still has the previous file open. It also records whether Rust's default Windows file sharing includes delete sharing or the adapter must request it explicitly.

Run from the repository root:

```powershell
mise exec -c "cargo run -p windows-atomic-replace-spike"
```
