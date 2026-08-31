import { useCallback, useEffect, useState } from "react";
import { ExternalLink, MessageCircleHeart } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { FeedbackCategory, FeedbackSummary } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { FeedbackForm } from "@/components/feedback-form";
import { FeedbackBoard } from "@/views/feedback/FeedbackBoard";
import { DiscordIcon } from "@/components/discord-icon";
import { FEEDBACK_CATEGORIES, timeAgo } from "@/lib/language";
import { DISCORD_INVITE_URL } from "@/lib/links";

const CATEGORY_LABELS = Object.fromEntries(
  FEEDBACK_CATEGORIES.map((c) => [c.value, c.label]),
) as Record<FeedbackCategory, string>;

/** Plain-language wording for the statuses the store records. */
const STATUS_LABELS: Record<string, string> = {
  open: "reported",
  duplicate: "merged with an existing report",
  forwarded: "sent to TinyHumans",
};

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/**
 * The feedback surface: report something, see it against everyone else's asks,
 * and follow what happens to it.
 *
 * Three layers, in the order an operator meets them:
 *
 * 1. **Flag something** — the local scrub-then-preview capture. Nothing leaves
 *    the machine until the operator has seen the exact text that would.
 * 2. **The shared board** — the hub's cross-product list of what has been
 *    asked for, with votes, replies and a triage status
 *    ([`FeedbackBoard`]). Absent on a host with no TinyHumans credential,
 *    which has no board to show.
 * 3. **Your reports** — this company's own captures, including the ones that
 *    never left.
 */
export function FeedbackView({ client, company }: Props) {
  // Stays null until /spec answers, so the copy below never flickers between
  // the provisioned and unprovisioned wordings.
  const [provisioned, setProvisioned] = useState<boolean | null>(null);
  const [reports, setReports] = useState<FeedbackSummary[] | null>(null);
  // Bumped on Done, which clears the form, refetches the reports list, and
  // re-anchors the board — a report that was just forwarded to the hub is a
  // board row now, and it should not take a page reload to see it.
  const [round, setRound] = useState(0);
  // Flipped off the first time the host says it has no board. Kept here rather
  // than inside the board so the copy above can stop promising one.
  const [hasBoard, setHasBoard] = useState(true);

  useEffect(() => {
    let live = true;
    client
      .spec()
      .then((spec) => live && setProvisioned(spec.cycles_available))
      // A host that cannot answer /spec is treated as unprovisioned.
      .catch(() => live && setProvisioned(false));
    return () => {
      live = false;
    };
  }, [client]);

  useEffect(() => {
    let live = true;
    client
      .listFeedback(company)
      .then((items) => live && setReports(items))
      // A host without the list route yet shows the form and nothing else.
      .catch(() => live && setReports([]));
    return () => {
      live = false;
    };
  }, [client, company, round]);

  const onDone = useCallback(() => setRound((n) => n + 1), []);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader
        title="Feedback"
        width="3xl"
        description={
          <>
            Flag a wrong result, a missing capability, or anything that felt off
            {hasBoard && " — then vote on what everyone else has asked for"}.
          </>
        }
      />
      <div className="mx-auto min-h-0 w-full max-w-3xl flex-1 space-y-6 overflow-y-auto px-4 py-6">
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Flag something</CardTitle>
            <CardDescription>
              You&apos;ll preview exactly what gets shared before it leaves your machine.
              {provisioned === true && " Reports go to your TinyHumans account."}
              {provisioned === false && " Reports stay here until you choose to file them."}
            </CardDescription>
          </CardHeader>
          <CardContent>
            {/* Remounting on `round` resets the form after a submission. */}
            <FeedbackForm
              key={round}
              client={client}
              company={company}
              onDone={onDone}
              showCancel={false}
            />
          </CardContent>
        </Card>

        {hasBoard && (
          <FeedbackBoard
            client={client}
            company={company}
            refreshKey={round}
            onAvailability={setHasBoard}
          />
        )}

        {reports !== null && reports.length > 0 && (
          <Card>
            <CardHeader>
              <CardTitle className="text-base">Your reports</CardTitle>
              <CardDescription>
                What this company has captured, newest first.
              </CardDescription>
            </CardHeader>
            <CardContent className="p-0">
              <ul className="divide-y">
                {reports.map((report) => (
                  <ReportRow key={report.id} report={report} />
                ))}
              </ul>
            </CardContent>
          </Card>
        )}

        <Card className="overflow-hidden">
          <CardContent className="flex flex-col items-start gap-4 sm:flex-row sm:items-center sm:justify-between">
            <div className="flex items-center gap-3">
              {/* Discord's own blurple, via the named token — see
                  `--brand-discord` in index.css for why it stays a fixed hex. */}
              <div className="flex size-11 shrink-0 items-center justify-center rounded-xl bg-(--brand-discord)/12 text-(--brand-discord)">
                <DiscordIcon className="size-6" />
              </div>
              <div>
                <p className="flex items-center gap-1.5 font-medium">
                  Join the community <MessageCircleHeart className="size-4 text-muted-foreground" />
                </p>
                <p className="text-sm text-muted-foreground">
                  Trade tips, share what your company built, and shape the roadmap.
                </p>
              </div>
            </div>
            <Button
              render={<a href={DISCORD_INVITE_URL} target="_blank" rel="noreferrer" />}
              className="w-full shrink-0 bg-(--brand-discord) text-white hover:bg-(--brand-discord-hover) sm:w-auto"
            >
              <DiscordIcon className="size-4" /> Join our Discord
            </Button>
          </CardContent>
        </Card>
      </div>
    </div>
  );
}

function ReportRow({ report }: { report: FeedbackSummary }) {
  return (
    <li className="flex items-start justify-between gap-4 px-6 py-3">
      <div className="min-w-0 space-y-0.5">
        <p className="truncate text-sm font-medium">
          {CATEGORY_LABELS[report.category] ?? report.category}
        </p>
        <p className="text-xs text-muted-foreground">
          {timeAgo(report.at_millis, Date.now())}
          {report.work_item && ` · ${report.work_item}`}
          {` · ${
            report.issue_status
              ? (STATUS_LABELS[report.issue_status] ?? report.issue_status)
              : "saved locally"
          }`}
        </p>
      </div>
      {report.filed_issue_url && (
        <a
          className="inline-flex shrink-0 items-center gap-1 text-xs font-medium underline underline-offset-4"
          href={report.filed_issue_url}
          target="_blank"
          rel="noreferrer"
        >
          View <ExternalLink className="size-3" />
        </a>
      )}
    </li>
  );
}
