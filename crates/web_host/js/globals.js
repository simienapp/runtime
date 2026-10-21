// Copyright 2018-2026 the Deno authors. MIT license.
// Adapted for Simien from Deno's runtime/js/98_global_scope_shared.js (v2.9.7).
//
// Puts WinterTC's Minimum Common API on globalThis from simien_web,
// simien_fetch and simien_crypto. Most globals are lazy: a module is
// evaluated the first time an app touches what it defines, so an isolate
// only pays for the APIs its app uses. `console` stays the runtime's own.

import { core, primordials } from "ext:core/mod.js";
const { SafeArrayIterator } = primordials;

const event = core.loadExtScript("ext:simien_web/02_event.js");
const { DOMException } = core.loadExtScript("ext:simien_web/01_dom_exception.js");
const abortSignal = core.loadExtScript("ext:simien_web/03_abort_signal.js");
const url = core.loadExtScript("ext:simien_web/00_url.js");
const file = core.loadExtScript("ext:simien_web/09_file.js");
const messagePort = core.loadExtScript("ext:simien_web/13_message_port.js");
const performance = core.loadExtScript("ext:simien_web/15_performance.js");
const crypto = core.loadExtScript("ext:simien_crypto/00_crypto.js");

const loadBase64 = () => core.loadExtScript("ext:simien_web/05_base64.js");
const loadEncoding = () => core.loadExtScript("ext:simien_web/08_text_encoding.js");
const loadUrlPattern = () => core.loadExtScript("ext:simien_web/01_urlpattern.js");
const loadTimers = () => core.loadExtScript("ext:simien_web/02_timers.js");
const lazyStreams = () => core.loadExtScript("ext:simien_web/06_streams.js");
const lazyCompression = () => core.loadExtScript("ext:simien_web/14_compression.js");
const loadHeaders = () => core.loadExtScript("ext:simien_fetch/20_headers.js");
const loadFormData = () => core.loadExtScript("ext:simien_fetch/21_formdata.js");
const lazyRequest = () => core.loadExtScript("ext:simien_fetch/23_request.js");
const lazyResponse = () => core.loadExtScript("ext:simien_fetch/23_response.js");
const lazyFetch = () => core.loadExtScript("ext:simien_fetch/26_fetch.js");

const USER_AGENT = "Simien";

const minimumCommonApi = {
  AbortController: core.propNonEnumerable(abortSignal.AbortController),
  AbortSignal: core.propNonEnumerable(abortSignal.AbortSignal),
  Blob: core.propNonEnumerable(file.Blob),
  ByteLengthQueuingStrategy: core.propNonEnumerableLazyLoaded(
    (s) => s.ByteLengthQueuingStrategy,
    lazyStreams,
  ),
  CompressionStream: core.propNonEnumerableLazyLoaded(
    (c) => c.CompressionStream,
    lazyCompression,
  ),
  CountQueuingStrategy: core.propNonEnumerableLazyLoaded(
    (s) => s.CountQueuingStrategy,
    lazyStreams,
  ),
  CryptoKey: core.propNonEnumerable(crypto.CryptoKey),
  CustomEvent: core.propNonEnumerable(event.CustomEvent),
  DecompressionStream: core.propNonEnumerableLazyLoaded(
    (c) => c.DecompressionStream,
    lazyCompression,
  ),
  DOMException: core.propNonEnumerable(DOMException),
  ErrorEvent: core.propNonEnumerable(event.ErrorEvent),
  Event: core.propNonEnumerable(event.Event),
  EventTarget: core.propNonEnumerable(event.EventTarget),
  File: core.propNonEnumerable(file.File),
  FormData: core.propNonEnumerableLazyLoaded((m) => m.FormData, loadFormData),
  Headers: core.propNonEnumerableLazyLoaded((m) => m.Headers, loadHeaders),
  MessageEvent: core.propNonEnumerable(event.MessageEvent),
  Performance: core.propNonEnumerable(performance.Performance),
  PerformanceEntry: core.propNonEnumerable(performance.PerformanceEntry),
  PerformanceMark: core.propNonEnumerable(performance.PerformanceMark),
  PerformanceMeasure: core.propNonEnumerable(performance.PerformanceMeasure),
  PromiseRejectionEvent: core.propNonEnumerable(event.PromiseRejectionEvent),
  ReadableStream: core.propNonEnumerableLazyLoaded(
    (s) => s.ReadableStream,
    lazyStreams,
  ),
  ReadableStreamDefaultReader: core.propNonEnumerableLazyLoaded(
    (s) => s.ReadableStreamDefaultReader,
    lazyStreams,
  ),
  Request: core.propNonEnumerableLazyLoaded(
    (r) => r.Request,
    lazyRequest,
  ),
  Response: core.propNonEnumerableLazyLoaded(
    (r) => r.Response,
    lazyResponse,
  ),
  TextDecoder: core.propNonEnumerableLazyLoaded(
    (m) => m.TextDecoder,
    loadEncoding,
  ),
  TextEncoder: core.propNonEnumerableLazyLoaded(
    (m) => m.TextEncoder,
    loadEncoding,
  ),
  TextDecoderStream: core.propNonEnumerableLazyLoaded(
    (m) => m.TextDecoderStream,
    loadEncoding,
  ),
  TextEncoderStream: core.propNonEnumerableLazyLoaded(
    (m) => m.TextEncoderStream,
    loadEncoding,
  ),
  TransformStream: core.propNonEnumerableLazyLoaded(
    (s) => s.TransformStream,
    lazyStreams,
  ),
  URL: core.propNonEnumerable(url.URL),
  URLPattern: core.propNonEnumerableLazyLoaded(
    (m) => m.URLPattern,
    loadUrlPattern,
  ),
  URLSearchParams: core.propNonEnumerable(url.URLSearchParams),
  MessageChannel: core.propNonEnumerable(messagePort.MessageChannel),
  MessagePort: core.propNonEnumerable(messagePort.MessagePort),
  WritableStream: core.propNonEnumerableLazyLoaded(
    (s) => s.WritableStream,
    lazyStreams,
  ),
  WritableStreamDefaultWriter: core.propNonEnumerableLazyLoaded(
    (s) => s.WritableStreamDefaultWriter,
    lazyStreams,
  ),
  WritableStreamDefaultController: core.propNonEnumerableLazyLoaded(
    (s) => s.WritableStreamDefaultController,
    lazyStreams,
  ),
  ReadableByteStreamController: core.propNonEnumerableLazyLoaded(
    (s) => s.ReadableByteStreamController,
    lazyStreams,
  ),
  ReadableStreamBYOBReader: core.propNonEnumerableLazyLoaded(
    (s) => s.ReadableStreamBYOBReader,
    lazyStreams,
  ),
  ReadableStreamBYOBRequest: core.propNonEnumerableLazyLoaded(
    (s) => s.ReadableStreamBYOBRequest,
    lazyStreams,
  ),
  ReadableStreamDefaultController: core.propNonEnumerableLazyLoaded(
    (s) => s.ReadableStreamDefaultController,
    lazyStreams,
  ),
  TransformStreamDefaultController: core.propNonEnumerableLazyLoaded(
    (s) => s.TransformStreamDefaultController,
    lazyStreams,
  ),
  atob: core.propWritableLazyLoaded((m) => m.atob, loadBase64),
  btoa: core.propWritableLazyLoaded((m) => m.btoa, loadBase64),
  clearInterval: core.propWritableLazyLoaded(
    (m) => m.clearInterval,
    loadTimers,
  ),
  clearTimeout: core.propWritableLazyLoaded(
    (m) => m.clearTimeout,
    loadTimers,
  ),
  crypto: core.propGetterOnly(() => crypto.crypto),
  Crypto: core.propNonEnumerable(crypto.Crypto),
  SubtleCrypto: core.propNonEnumerable(crypto.SubtleCrypto),
  fetch: core.propWritable(function fetch(...args) {
    return lazyFetch().fetch(...new SafeArrayIterator(args));
  }),
  performance: core.propWritable(performance.performance),
  reportError: core.propWritable(event.reportError),
  structuredClone: core.propWritable(messagePort.structuredClone),
  setInterval: core.propWritableLazyLoaded((m) => m.setInterval, loadTimers),
  setTimeout: core.propWritableLazyLoaded((m) => m.setTimeout, loadTimers),
  self: core.propGetterOnly(() => globalThis),
  navigator: core.propNonEnumerable({ userAgent: USER_AGENT }),
};

core.defineGlobalProperties(globalThis, minimumCommonApi);

