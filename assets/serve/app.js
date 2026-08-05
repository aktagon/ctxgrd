// ctxgrd serve — live-reload poller (ADR-097 § SRV-007).
// Polls a lightweight change endpoint and reloads the page when the
// governed set's digest changes. No WebSocket, no SSE — a vanilla layer
// over the hand-rolled server.

(function () {
  "use strict";

  var POLL_MS = 1000;
  var known = null;

  function poll() {
    fetch("/reload", { cache: "no-store" })
      .then(function (r) {
        return r.ok ? r.json() : null;
      })
      .then(function (data) {
        if (!data || typeof data.digest === "undefined") {
          return;
        }
        if (known === null) {
          known = data.digest;
        } else if (data.digest !== known) {
          window.location.reload();
        }
      })
      .catch(function () {
        // Server gone or transient error — keep polling quietly.
      });
  }

  setInterval(poll, POLL_MS);
  poll();
})();
