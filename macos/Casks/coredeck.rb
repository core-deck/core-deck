cask "coredeck" do
  version "0.2.0"
  sha256 "6396ba3881928f157ae013f719b81d5958b6954d6a85074374b1811e8fd6cf83"

  url "https://github.com/core-deck/core-deck/releases/download/v#{version}/CoreDeck-#{version}.dmg"
  name "Core Deck"
  desc "Hardware control surface for Claude Code (daemon + claude wrapper)"
  homepage "https://github.com/core-deck/core-deck"

  livecheck do
    url :url
    strategy :github_latest
  end

  auto_updates false
  # Prebuilt DMG is Apple Silicon only (Intel was dropped from CI).
  # Intel users build from source — see docs/Building.md.
  depends_on arch: :arm64
  depends_on macos: :big_sur

  app "Core Deck.app"
  # Expose both binaries on PATH. The wrapper is what users alias `claude`
  # to; the daemon binary is also exposed so `coredeck setup`,
  # `coredeck hooks install`, etc. work from any shell.
  binary "#{appdir}/Core Deck.app/Contents/MacOS/coredeck"
  binary "#{appdir}/Core Deck.app/Contents/MacOS/coredeck-claude"

  # Stop and unload the launchd agent before the app is removed so
  # `brew uninstall` doesn't leave a zombie daemon running.
  uninstall launchctl: "com.coredeck.daemon",
            quit:      "com.coredeck.CoreDeck"

  # `brew uninstall --zap` removes user-level state too: launchd plist,
  # daemon logs, the embedded hook scripts, and the daemon data dir
  # (wrapper-id cache + soft-key presets).
  #
  # NOTE: this does NOT remove CoreDeck's hook *entries* from
  # ~/.claude/settings.json (they'd be left pointing at the deleted
  # scripts). Run `coredeck hooks uninstall` before `brew uninstall` for
  # a fully clean removal.
  zap trash: [
    "~/.claude/coredeck-hook.sh",
    "~/.claude/coredeck-register.sh",
    "~/Library/Application Support/com.coredeck.CoreDeck",
    "~/Library/LaunchAgents/com.coredeck.daemon.plist",
    "~/Library/Logs/coredeck.log",
  ]

  caveats <<~EOS
    Finish setup by running:

      coredeck setup

    That installs the Claude Code hooks (~/.claude/settings.json) and
    registers the launchd agent so the daemon auto-starts at login.

    To run Claude under CoreDeck, alias `claude` to the wrapper in your
    shell rc:

      alias claude="coredeck-claude"
  EOS
end
