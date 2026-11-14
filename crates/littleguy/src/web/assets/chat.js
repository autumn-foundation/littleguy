// The chat of littleguy.
//
// The page CSP allows no inline script, so this file holds the behaviour of
// the chat. Enter sends the message. Shift+Enter breaks the line. The message
// shows at once, and the thread keeps its newest message in view.
(() => {
  "use strict";

  const form = document.getElementById("composer");
  const scroller = document.getElementById("scroller");
  const thread = document.getElementById("thread");
  if (!form || !scroller || !thread) {
    return;
  }
  const box = form.querySelector("textarea");

  const toBottom = () => {
    scroller.scrollTop = scroller.scrollHeight;
  };

  // The box grows with its text, up to the height that the CSS allows.
  const fit = () => {
    box.style.height = "auto";
    box.style.height = `${box.scrollHeight}px`;
  };

  box.addEventListener("input", fit);

  box.addEventListener("keydown", (event) => {
    // An input method composes a character with Enter. That Enter does not
    // send.
    if (event.key !== "Enter" || event.shiftKey || event.isComposing) {
      return;
    }
    event.preventDefault();
    if (box.value.trim() !== "") {
      form.requestSubmit();
    }
  });

  // htmx reads the form before this event, so the box can clear here. The
  // message is text, so it goes in with textContent and never as HTML.
  form.addEventListener("htmx:beforeRequest", () => {
    const message = document.createElement("div");
    message.className = "msg you";
    const body = document.createElement("div");
    body.className = "body";
    body.textContent = box.value;
    message.append(body);
    thread.querySelector(".empty")?.remove();
    thread.append(message);
    box.value = "";
    fit();
    toBottom();
  });

  document.body.addEventListener("htmx:afterSettle", toBottom);
  // The markdown of an answer mounts after the swap, and it can grow.
  document.addEventListener("markstream:mount", toBottom);
  form.addEventListener("htmx:afterRequest", () => box.focus());

  toBottom();
  box.focus();
})();
