# forge-plugin-bases

## Using in Forge

Add the package to your Forge `init.rhai` (replace the Git URL with this
repository's URL and set `vault` to your notes directory):

```rhai
use_package("bases", #{
    git: "https://github.com/martial-plains/bases.forge",
    config: #{ vault: "~/Notes", column_width: 32 },
    grant: #{ directories: ["~/Notes"] },
});
```

Forge installs and loads the package from this declaration. The directory
grant lets the plugin read and update notes in the configured vault.

## Building

This plugin is built against the Forge plugin SDK (`forge-plugin-sdk`),
taken from the Forge repository named in `Cargo.toml` (change the `git =`
URL if Forge lives elsewhere).

    rustup target add wasm32-wasip2
    cargo build --release --target wasm32-wasip2

Copy the resulting `.wasm` to `~/.config/forge/plugins/`.

## License

GPL-3.0-or-later. See the license file in this directory.
