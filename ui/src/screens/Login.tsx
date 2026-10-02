import { useState, type FormEvent } from "react";
import { setKey } from "@/lib/auth";
import { verifyKey } from "@/lib/api";
import { Brand } from "@/ui/Brand";
import { Button, Input } from "@/ui/primitives";

export default function Login() {
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    const key = value.trim();
    if (!key) return;
    setBusy(true);
    setError(null);
    try {
      await verifyKey(key);
      setKey(key);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Sign in failed");
      setBusy(false);
    }
  }

  return (
    <div className="flex h-full items-center justify-center px-6">
      <form onSubmit={submit} className="enter-fade grid w-[320px] gap-5">
        <Brand large />
        <div className="grid gap-2">
          <Input
            autoFocus
            type="password"
            placeholder="Management key"
            aria-label="Management key"
            value={value}
            onChange={(e) => {
              setValue(e.target.value);
              setError(null);
            }}
            className="h-9"
            aria-invalid={error !== null}
          />
          <div className="h-4 text-[12px] text-bad" role="alert">
            {error}
          </div>
        </div>
        <Button type="submit" variant="primary" disabled={busy || !value.trim()} className="h-9 w-full">
          {busy ? "Checking" : "Sign in"}
        </Button>
      </form>
    </div>
  );
}
