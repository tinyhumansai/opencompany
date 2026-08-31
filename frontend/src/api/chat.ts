import type { OpenCompanyClient } from "./client";
import type { AttachmentDto } from "./types";

/**
 * Upload one file to attach to a chat message (issue #1682).
 *
 * The byte-transfer half of a two-step send: the file's bytes go up here on a
 * multipart route, and the returned reference is what the ordinary JSON `/chat`
 * message then carries by `nodeId`. Decoupling the two keeps the synchronous,
 * turn-running `/chat` POST off the bytes.
 *
 * `postForm` rather than `post`: the shared request helper sets a JSON
 * content-type and `JSON.stringify`s its body, and a multipart upload must let
 * the browser set the boundary itself — the same reason `uploadFile` reaches
 * for it in `api/workspace.ts`.
 *
 * Every field on the answer is the **store's** — the id it generated, the name
 * it stored under, the mime it resolved, the length it measured — so the
 * composer draws a chip from what was actually stored, and the send path
 * carries only the id the host will re-resolve against this company's own tree.
 */
export async function uploadChatAttachment(
  client: OpenCompanyClient,
  company: string | null,
  file: File,
): Promise<AttachmentDto> {
  const form = new FormData();
  form.append("file", file, file.name);
  return client.postForm<AttachmentDto>(`${client.scopeFor(company)}/chat/upload`, form);
}
