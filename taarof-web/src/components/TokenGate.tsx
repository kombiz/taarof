import { FormEvent, useState } from "react";

interface TokenGateProps {
  errorMessage?: string | null;
  onSubmit: (token: string) => void;
}

export function TokenGate({ errorMessage, onSubmit }: TokenGateProps) {
  const [token, setToken] = useState("");

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    onSubmit(token.trim());
  }

  return (
    <main className="token-gate">
      <section className="token-card">
        <div className="token-card__eyebrow">Local access</div>
        <h1>Connect this browser to taarof</h1>
        <p>
          The web client is local and starts in observe mode. Enter the bearer
          token from the running taarof HTTP server to connect this browser to
          your workspace.
        </p>
        <form className="token-form" onSubmit={handleSubmit}>
          <label className="token-form__label" htmlFor="taarof-token">
            Bearer token
          </label>
          <input
            id="taarof-token"
            className="token-form__input"
            autoFocus
            autoComplete="off"
            spellCheck={false}
            placeholder="Paste taarof token"
            value={token}
            onChange={(event) => setToken(event.target.value)}
          />
          <button className="token-form__submit" type="submit">
            Open web shell
          </button>
        </form>
        <p className="token-card__hint">
          You can also open the client with <code>?token=...</code>. Invalid
          tokens are cleared automatically after a `401` from the taarof API.
        </p>
        {errorMessage ? <p className="token-card__error">{errorMessage}</p> : null}
      </section>
    </main>
  );
}
