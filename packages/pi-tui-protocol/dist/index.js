/**
 * Pure TypeScript protocol client for the pi extension-host / structured UI
 * bridge. Speaks UTF-8 JSONL frames only; no terminal, ANSI, or native access.
 */
export { decodeFrameLine, decodeFrameStr, decodeFrameStrStrict, encodeFrame, encodeFrameString, errorFrame, eventFrame, FrameDecoder, ProtocolError, requestFrame, responseFrame, validateFrame, } from "./codec.js";
export { ProtocolClient, } from "./client.js";
export { COMPATIBILITY_VERSION, isMethod, localHello, localHelloAck, MAX_FRAME_BYTES, METHODS, PROTOCOL_VERSION, } from "./types.js";
//# sourceMappingURL=index.js.map