import { api, saveBlob } from "@/lib/api";
import { credentialTitle } from "@/lib/credential";
import { toast } from "@/lib/toast";
import type { CredentialFile } from "@/lib/types";
import { confirm } from "@/ui/overlays";

export async function downloadCredential(name: string) {
  saveBlob(await api.getBlob("/credentials/download", { name }), name);
}

/** Confirms, then deletes. Resolves false when the user backs out. */
export async function deleteCredential(f: CredentialFile): Promise<boolean> {
  const ok = await confirm({
    title: "Delete credential",
    body: `${credentialTitle(f)} (${f.name}) will be removed from the auth directory.`,
    confirm: "Delete",
    danger: true,
  });
  if (!ok) return false;
  await api.del("/credentials", { name: f.name });
  toast.ok("Credential deleted");
  return true;
}
