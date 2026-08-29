# Negative mutation fixture

Not registered in the lane corpus. Used only by mutation tests.

```rust
use pi_ai::does_not_exist;

fn main() {
    let _ = does_not_exist();
}
```

```ts
import { doesNotExist } from "@earendil-works/pi-tui-protocol";

const value = doesNotExist();
void value;
```
