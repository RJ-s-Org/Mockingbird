# The README demo. Record from the repository root with a built binary on PATH
# and OPENROUTER_API_KEY set (about a cent of tokens):
#
#   PATH="$PWD/target/release:$PATH" vhs docs/assets/demo.tape
#
# Once `mockingbird run` has landed, a second scene can show
# `mockingbird run -- node app.js` against a mapped host.
Output docs/assets/demo.gif

Require mockingbird
Require curl
Require jq

Set Shell zsh
Set FontSize 15
Set Width 1280
Set Height 640
Set Padding 18
Set TypingSpeed 35ms
Set Theme "Catppuccin Mocha"

Hide
Type "cd docs/assets/demo && rm -rf .mockingbird && setopt interactivecomments && export PS1='$ ' && clear" Enter
Type "mockingbird server start -c mockingbird.yaml >/dev/null && sleep 2 && clear" Enter
Sleep 4s
Show

Type "# A bookshop API from its OpenAPI contract, answered by an LLM" Enter
Sleep 600ms
Type "curl -s 'localhost:8080/books?limit=3' | jq -c '.data[] | {title, author, price}'" Enter
Wait+Screen@40s /amount/
Sleep 3s

Type "# Create an order: the response follows the contract" Enter
Type `curl -s -XPOST localhost:8080/orders -H 'content-type: application/json' -d '{"bookId":"b1","quantity":2,"customerEmail":"ada@example.com"}' | tee order.json | jq -c` Enter
Wait+Screen@40s /createdAt/
Sleep 3s
Hide
Type "ID=$(jq -r .id order.json) && rm order.json" Enter
Show

Type "# Read it back: answered from session state, no LLM call" Enter
Type "curl -s -D - localhost:8080/orders/$ID | grep -E '^HTTP|^x-mockingbird-source|^[{]'" Enter
Sleep 2500ms

Type "# Ask for the failure path: the 404 the contract declares" Enter
Type "curl -s localhost:8080/orders/$ID -H 'X-Mockingbird-Status: 404' | jq -c" Enter
Wait+Screen@40s /message/
Sleep 4s

Hide
Type "mockingbird server stop >/dev/null" Enter
