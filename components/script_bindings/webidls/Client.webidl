/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/ServiceWorker/#client

[Pref="dom_serviceworker_enabled", Exposed=ServiceWorker]
interface Client {
  readonly attribute USVString url;
  readonly attribute FrameType frameType;
  readonly attribute DOMString id;
  //void postMessage(any message, optional sequence<Transferable> transfer);
};

enum FrameType {
  "auxiliary",
  "top-level",
  "nested",
  "none"
};
