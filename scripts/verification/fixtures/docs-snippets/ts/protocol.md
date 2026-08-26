# pi-tui-protocol fixture

The issue names a `Codec` export that does not exist. This fence covers the
codec function set and protocol constants from the shipped package entrypoint.

```ts
import {
	COMPATIBILITY_VERSION,
	FrameDecoder,
	METHODS,
	PROTOCOL_VERSION,
	ProtocolClient,
	decodeFrameLine,
	encodeFrame,
	isMethod,
	validateFrame,
} from "@earendil-works/pi-tui-protocol";
import type { Frame, Method } from "@earendil-works/pi-tui-protocol";

const version: number = PROTOCOL_VERSION;
const compatibility: string = COMPATIBILITY_VERSION;
const methods: readonly Method[] = METHODS;
const helloIsMethod = isMethod("hello");
const frame: Frame = { id: 1, kind: "req", method: "hello", payload: {} };
validateFrame(frame);
const encoded = encodeFrame(frame);
const decoded = decodeFrameLine(encoded);
const decoder = new FrameDecoder();
const ClientCtor: typeof ProtocolClient = ProtocolClient;

void version;
void compatibility;
void methods;
void helloIsMethod;
void decoded;
void decoder;
void ClientCtor;
```
