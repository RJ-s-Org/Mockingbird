// The app in the README demo: it calls the real host and knows nothing about mocks.
const api = "https://api.bookshop.example";

const books = await (await fetch(`${api}/books?limit=3`)).json();
for (const b of books.data) console.log(`${b.title} (${b.author}): ${b.price.amount} ${b.price.currency}`);

const order = await (await fetch(`${api}/orders`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ bookId: books.data[0].id, quantity: 2, customerEmail: "ada@example.com" }),
})).json();
console.log("created", order.id, order.status);

const again = await fetch(`${api}/orders/${order.id}`);
console.log("read back", (await again.json()).id, "from", again.headers.get("x-mockingbird-source"));

const missing = await fetch(`${api}/orders/${order.id}`, { headers: { "X-Mockingbird-Status": "404" } });
console.log(missing.status, JSON.stringify(await missing.json()));
