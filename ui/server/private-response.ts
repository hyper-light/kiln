/** Dynamic HTML/functions may carry a private loader; hashed static files use their own policy. */
export function privateResponse(response: Response): Response {
  const headers = new Headers(response.headers);
  headers.set("cache-control", "private, no-store");
  const vary = headers.get("vary");
  if (vary !== "*") headers.set("vary", [...new Set((vary ? vary.split(",") : []).map((s) => s.trim()).concat("Cookie", "Authorization"))].join(", "));
  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers,
  });
}
