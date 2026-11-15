// The MCP consent page: one passkey assertion over the statement Arca minted signs the owner in and approves the app.
"use strict";

const request = document.querySelector("main").dataset.request;
const enc = new TextEncoder();

function b64url(bytes) {
  let text = "";
  for (const byte of bytes) text += String.fromCharCode(byte);
  return btoa(text).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function fromB64url(value) {
  const text = atob(value.replace(/-/g, "+").replace(/_/g, "/"));
  return Uint8Array.from(text, (c) => c.charCodeAt(0));
}

function status(text, tone) {
  const target = document.getElementById("status");
  target.textContent = text;
  target.dataset.tone = tone || "";
}

function buttons(enabled) {
  document.getElementById("approve").disabled = !enabled;
  document.getElementById("deny").disabled = !enabled;
}

async function post(path, body) {
  const answer = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const result = await answer.json().catch(() => ({}));
  if (!answer.ok) throw new Error(result.message || "Arca answered " + answer.status);
  return result;
}

async function approve() {
  buttons(false);
  status("Confirm with your passkey…", "");
  try {
    const begun = await post("/oauth/authorize/begin", { id: request });
    const challenge = await crypto.subtle.digest(
      "SHA-256",
      enc.encode("arca-kernel-approval/v1\narca-kernel-approve\n" + begun.statement),
    );
    const credential = await navigator.credentials.get({
      publicKey: {
        challenge,
        rpId: begun.rpId,
        userVerification: "required",
        timeout: 60000,
        allowCredentials: begun.allowCredentials.map((c) => ({ type: "public-key", id: fromB64url(c.id) })),
      },
    });
    if (!credential) throw new Error("no passkey was selected");
    const response = credential.response;
    const proof = {
      alg: response.signature.byteLength > 72 ? "RS256" : "ES256",
      authenticatorData: b64url(new Uint8Array(response.authenticatorData)),
      clientDataJSON: b64url(new Uint8Array(response.clientDataJSON)),
      credentialId: b64url(new Uint8Array(credential.rawId)),
      kind: "webauthn",
      signature: b64url(new Uint8Array(response.signature)),
      v: 1,
    };
    const done = await post("/oauth/authorize/approve", { id: request, statement: begun.statement, proof });
    status("Approved. Taking you back to the app…", "ok");
    location.assign(done.redirect);
  } catch (error) {
    const name = error && error.name;
    status(name === "NotAllowedError" ? "The passkey prompt was closed. Nothing was approved." : String(error.message || error), "bad");
    buttons(true);
  }
}

async function deny() {
  buttons(false);
  try {
    const done = await post("/oauth/authorize/deny", { id: request });
    status("Not allowed. Taking you back to the app…", "ok");
    location.assign(done.redirect);
  } catch (error) {
    status(String(error.message || error), "bad");
    buttons(true);
  }
}

document.getElementById("approve").addEventListener("click", approve);
document.getElementById("deny").addEventListener("click", deny);
