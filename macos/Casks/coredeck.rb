cask "coredeck" do
  version "0.1.0"
  sha256 :no_check # replace with the released DMG sha256 once 0.1.0 ships

  url "https://github.com/core-deck/core-deck/releases/download/v#{version}/CoreDeck-#{version}.dmg"
  name "Core Deck"
  desc "Hardware control surface for Claude Code (daemon + claude wrapper)"
  homepage "https://github.com/core-deck/core-deck"

  livecheck do
    url :url
    strategy :github_latest
  end

  auto_updates false
  depends_on macos: ">= :big_sur"

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
  # Claude Code hook config, daemon logs, settings cache.
  zap trash: [
    "~/Library/LaunchAgents/com.coredeck.daemon.plist",
    "~/Library/Logs/coredeck.log",
    "~/.claude/coredeck-hook.sh",
    "~/.claude/coredeck-register.sh",
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
