# The README demo. Record from the repository root with a built binary on PATH
# and OPENROUTER_API_KEY set (about a cent of tokens):
#
#   PATH="$PWD/target/release:$PATH" vhs docs/assets/demo.tape
#
# An unmodified Node app (demo/shop.mjs) calls https://api.bookshop.example;
# `mockingbird run` sends those calls to the bookshop mock.
Output docs/assets/demo.gif

Require mockingbird
Require node

Set Shell zsh
Set FontSize 15
Set Width 1280
Set Height 640
Set Padding 18
Set TypingSpeed 35ms
Set Theme "Catppuccin Mocha"

Hide
Type "cd docs/assets/demo && rm -rf .mockingbird && setopt interactivecomments && export PS1='$ ' && clear" Enter
Show

Type "# A bookshop mock from its OpenAPI contract, mapped to the host it stands in for" Enter
Type "grep -A3 '^  bookshop:' mockingbird.yaml" Enter
Sleep 3s

Type "# An app that calls https://api.bookshop.example, unchanged" Enter
Type "head -5 shop.mjs" Enter
Sleep 2500ms

Type "# Run it against the mock: an LLM writes the answers, session state keeps them" Enter
Type "mockingbird run -- node shop.mjs" Enter
Wait+Screen@60s /404 \{/
Sleep 6s
