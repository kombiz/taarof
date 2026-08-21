import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "./App";
import "./styles.css";

if ("serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    void navigator.serviceWorker
      .getRegistrations()
      .then((registrations) => {
        return Promise.all(
          registrations.map((registration) => registration.unregister()),
        );
      })
      .catch((error: unknown) => {
        console.warn("taarof web: service worker cleanup failed", error);
      });
  });
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
