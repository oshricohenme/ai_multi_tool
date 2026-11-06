#!/usr/bin/env python3
"""The shop database for the MCP data tasks (D1-D3): seeded, so every run gets the same data.
usage: gen_shop_db.py <out.db>"""
import sqlite3, random, sys, datetime as dt, os
p = sys.argv[1]
if os.path.exists(p): os.remove(p)
r = random.Random(42); c = sqlite3.connect(p)
c.executescript("""
CREATE TABLE customers(id INTEGER PRIMARY KEY, name TEXT, email TEXT, country TEXT, signup_date TEXT, segment TEXT);
CREATE TABLE products(id INTEGER PRIMARY KEY, name TEXT, category TEXT, price_cents INTEGER);
CREATE TABLE orders(id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customers(id), created_at TEXT, status TEXT, channel TEXT);
CREATE TABLE order_items(order_id INTEGER REFERENCES orders(id), product_id INTEGER REFERENCES products(id), qty INTEGER, unit_price_cents INTEGER);
CREATE TABLE refunds(id INTEGER PRIMARY KEY, order_id INTEGER REFERENCES orders(id), product_id INTEGER REFERENCES products(id), amount_cents INTEGER, reason TEXT, created_at TEXT);
""")
countries = ["US","CA","GB","DE","FR","IL","IN","BR","JP","AU","MX","ES"]; w = [30,8,10,9,7,4,8,6,5,5,4,4]
first = ["Ava","Noah","Mia","Liam","Zoe","Eli","Noa","Omar","Yuki","Lena","Ivan","Sara","Tom","Ana","Raj","Li"]; last = ["Cohen","Smith","Garcia","Kim","Müller","Rossi","Levi","Patel","Silva","Tanaka","Brown","Dubois"]
for i in range(1, 3001):
    n = f"{r.choice(first)} {r.choice(last)}"; co = r.choices(countries, w)[0]
    c.execute("INSERT INTO customers VALUES(?,?,?,?,?,?)", (i, n, f"{n.split()[0].lower()}{i}@example.com", co, (dt.date(2024,1,1)+dt.timedelta(days=r.randrange(900))).isoformat(), r.choices(["consumer","pro","enterprise"],[75,20,5])[0]))
cats = {"audio":["Headphones","Speaker","Earbuds","Mic","DAC"],"home":["Lamp","Kettle","Blender","Fan","Heater"],"outdoor":["Tent","Backpack","Bottle","Stove","Jacket"],"office":["Chair","Desk","Monitor","Keyboard","Mouse"],"kids":["Puzzle","Robot","Blocks","Kite","Scooter"]}
pid = 0
for cat, names in cats.items():
    for nm in names:
        for v in ["Lite","Pro","Max","Mini","Plus","Air","S","X"]:
            pid += 1; c.execute("INSERT INTO products VALUES(?,?,?,?)", (pid, f"{nm} {v}", cat, r.randrange(900, 49900)))
prices = {row[0]: row[1] for row in c.execute("SELECT id, price_cents FROM products")}
oid = 0; rid = 0
for _ in range(30000):
    oid += 1; cu = r.randrange(1, 3001); d = dt.datetime(2025,7,1)+dt.timedelta(minutes=r.randrange(60*24*490))
    st = r.choices(["delivered","shipped","cancelled","returned"],[80,8,7,5])[0]
    c.execute("INSERT INTO orders VALUES(?,?,?,?,?)", (oid, cu, d.isoformat(timespec="seconds"), st, r.choices(["web","app","store"],[55,35,10])[0]))
    for _ in range(r.choices([1,2,3,4,5],[40,30,15,10,5])[0]):
        p_ = r.randrange(1, pid+1); q = r.choices([1,2,3],[80,15,5])[0]; up = int(prices[p_]*r.uniform(0.85,1.0))
        c.execute("INSERT INTO order_items VALUES(?,?,?,?)", (oid, p_, q, up))
        co = c.execute("SELECT country FROM customers WHERE id=?", (cu,)).fetchone()[0]
        rate = 0.03+(0.05 if co=="DE" else 0)+(0.04 if p_%17==0 else 0)
        if st in ("delivered","returned") and r.random() < rate*(4 if st=="returned" else 1):
            rid += 1; c.execute("INSERT INTO refunds VALUES(?,?,?,?,?,?)", (rid, oid, p_, up*q, r.choice(["damaged","late","wrong item","not as described","changed mind"]), (d+dt.timedelta(days=r.randrange(3,30))).isoformat(timespec="seconds")))
c.commit()
for t in ["customers","products","orders","order_items","refunds"]: print(t, c.execute(f"select count(*) from {t}").fetchone()[0])
