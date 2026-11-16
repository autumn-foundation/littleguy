// The chat of littleguy.
//
// The page CSP allows no inline script, so this file holds the behaviour of
// the chat. Enter sends the message. Shift+Enter breaks the line. The message
// shows at once. The answer then streams in, and the thread follows it.
(() => {
  "use strict";

  const form = document.getElementById("composer");
  const scroller = document.getElementById("scroller");
  const thread = document.getElementById("thread");
  const thinking = document.getElementById("thinking");
  if (!form || !scroller || !thread || !thinking) {
    return;
  }
  const box = form.querySelector("textarea");
  const send = form.querySelector("button");

  // The thread follows new text while the reader is at its end. A reader
  // who scrolls up to read keeps the place.
  let follow = true;
  scroller.addEventListener("scroll", () => {
    const gap = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
    follow = gap < 48;
  });
  const toBottom = () => {
    scroller.scrollTop = scroller.scrollHeight;
  };
  const keepUp = () => {
    if (follow) {
      toBottom();
    }
  };

  // The box grows with its text, up to the height that the CSS allows.
  const fit = () => {
    box.style.height = "auto";
    box.style.height = `${box.scrollHeight}px`;
  };

  // From the send until the answer settles, the chat is busy. The dots
  // show until the first text of the answer.
  let busy = false;
  const setBusy = (on) => {
    busy = on;
    send.disabled = on;
    thinking.classList.toggle("on", on);
  };

  box.addEventListener("input", fit);

  box.addEventListener("keydown", (event) => {
    // An input method composes a character with Enter. That Enter does not
    // send.
    if (event.key !== "Enter" || event.shiftKey || event.isComposing) {
      return;
    }
    event.preventDefault();
    if (!busy && box.value.trim() !== "") {
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
    setBusy(true);
    follow = true;
    toBottom();
  });

  // A send that fails gets no answer to wait for.
  form.addEventListener("htmx:responseError", () => setBusy(false));
  form.addEventListener("htmx:sendError", () => setBusy(false));

  // The calls that wait show as cards under the thread. An answer may
  // leave one, so the cards load again after each answer.
  const cards = () => {
    if (document.getElementById("pending")) {
      htmx.ajax("GET", "/chat/pending", { target: "#pending", swap: "outerHTML" });
    }
  };

  // The answer settles with its whole text, or fails.
  const settle = () => {
    setBusy(false);
    cards();
    keepUp();
    box.focus();
  };
  thread.addEventListener("markstream:done", settle);
  thread.addEventListener("markstream:error", settle);

  // Each piece of text changes the thread. Text hides the dots. A model
  // call that starts again, after a tool, clears the text, so the dots
  // show again.
  new MutationObserver(() => {
    const live = thread.querySelector('[data-markstream-state="streaming"]');
    if (live) {
      thinking.classList.toggle("on", busy && live.textContent.trim() === "");
    }
    keepUp();
  }).observe(scroller, { childList: true, subtree: true, characterData: true });

  toBottom();
  box.focus();
})();
