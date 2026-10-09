export const EXAMPLES = [
  {
    name: "JSON · user profile",
    old: { text: `{
  "id": 42,
  "name": "Ada Lovelace",
  "email": "ada@example.com",
  "roles": ["admin", "editor"],
  "address": { "city": "London", "zip": "N1 9GU" },
  "active": true
}
`, hint: "old.json" },
    new: { text: `{
  "id": 42,
  "name": "Ada King",
  "email": "ada@example.com",
  "roles": ["admin", "editor", "reviewer"],
  "address": { "city": "Ockham", "zip": "KT11 1AA", "country": "UK" },
  "preferences": { "theme": "dark" }
}
`, hint: "new.json" },
  },
  {
    name: "YAML · Kubernetes deployment",
    old: { text: `apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 2
  template:
    spec:
      containers:
        - name: web
          image: nginx:1.24
          ports:
            - containerPort: 80
`, hint: "old.yaml" },
    new: { text: `apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  labels:
    tier: frontend
spec:
  replicas: 4
  template:
    spec:
      containers:
        - name: web
          image: nginx:1.27
          ports:
            - containerPort: 8080
          resources:
            limits:
              memory: 256Mi
`, hint: "new.yaml" },
  },
  {
    name: "TOML · Cargo manifest",
    old: { text: `[package]
name = "demo"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1.0"
regex = "1.5"
`, hint: "old.toml" },
    new: { text: `[package]
name = "demo"
version = "0.2.0"
edition = "2021"

[dependencies]
serde = "1.0"
toml = "0.8"

[features]
default = ["fast"]
fast = []
`, hint: "new.toml" },
  },
  {
    name: "XML · catalog",
    old: { text: `<catalog>
  <book id="1"><title>Rust in Action</title><price>39.99</price></book>
  <book id="2"><title>Programming Rust</title><price>49.99</price></book>
</catalog>
`, hint: "old.xml" },
    new: { text: `<catalog>
  <book id="1"><title>Rust in Action</title><price>34.99</price></book>
  <book id="2"><title>Programming Rust, 2nd ed.</title><price>49.99</price></book>
  <book id="3"><title>Zero To Production</title><price>29.99</price></book>
</catalog>
`, hint: "new.xml" },
  },
  {
    name: "JSON · array reorder (use Array key: id)",
    keys: "id",
    old: { text: `{
  "users": [
    { "id": 1, "name": "Ada", "role": "admin" },
    { "id": 2, "name": "Grace", "role": "editor" },
    { "id": 3, "name": "Linus", "role": "viewer" }
  ]
}
`, hint: "old.json" },
    new: { text: `{
  "users": [
    { "id": 0, "name": "Alan", "role": "viewer" },
    { "id": 3, "name": "Linus", "role": "editor" },
    { "id": 1, "name": "Ada", "role": "admin" },
    { "id": 2, "name": "Grace", "role": "editor" }
  ]
}
`, hint: "new.json" },
  },
  {
    name: "JSON · ignore noisy fields (Ignore: /**/updatedAt, /meta)",
    ignore: "/**/updatedAt, /meta",
    old: { text: `{
  "meta": { "requestId": "a1", "servedBy": "web-1" },
  "orders": [
    { "id": 1, "status": "open", "updatedAt": "2026-10-01T09:00:00Z" },
    { "id": 2, "status": "open", "updatedAt": "2026-10-01T09:05:00Z" }
  ]
}
`, hint: "old.json" },
    new: { text: `{
  "meta": { "requestId": "b7", "servedBy": "web-4" },
  "orders": [
    { "id": 1, "status": "shipped", "updatedAt": "2026-10-08T14:30:00Z" },
    { "id": 2, "status": "open", "updatedAt": "2026-10-08T14:31:00Z" }
  ]
}
`, hint: "new.json" },
  },
  {
    name: "Cross-format · YAML vs JSON",
    old: { text: `server:
  host: localhost
  port: 8080
debug: false
`, hint: "config.yaml" },
    new: { text: `{
  "server": { "host": "0.0.0.0", "port": 8080 },
  "debug": true,
  "workers": 4
}
`, hint: "config.json" },
  },
];
