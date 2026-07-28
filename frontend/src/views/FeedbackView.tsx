import { useCallback, useEffect, useState } from "react";
import { ExternalLink, MessageCircleHeart } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { FeedbackCategory, FeedbackSummary } from "@/api/types";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { FeedbackForm } from "@/components/feedback-form";
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

/** A standalone feedback surface: report something, see past reports, get help. */
export function FeedbackView({ client, company }: Props) {
  // Stays null until /spec answers, so the copy below never flickers between
  // the provisioned and unprovisioned wordings.
  const [provisioned, setProvisioned] = useState<boolean | null>(null);
  const [reports, setReports] = useState<FeedbackSummary[] | null>(null);
  // Bumped on Done, which both clears the form and refetches the list.
  const [round, setRound] = useState(0);

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
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-2xl space-y-6 px-4 py-6">
        <div className="space-y-1">
          <h2 className="text-2xl font-semibold tracking-tight">Feedback</h2>
          <p className="text-sm text-muted-foreground">
            Flag a wrong result, a missing capability, or anything that felt off.
          </p>
        </div>

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

        {reports !== null && reports.length > 0 && (
          <Card>
            <CardHeader>
              <CardTitle className="text-base">Your reports</CardTitle>
              <CardDescription>
                What you&apos;ve flagged from this company, newest first.
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
          <CardContent className="flex flex-col items-start gap-4 py-6 sm:flex-row sm:items-center sm:justify-between">
            <div className="flex items-center gap-3">
              <div className="flex size-11 shrink-0 items-center justify-center rounded-xl bg-[#5865F2]/12 text-[#5865F2]">
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
              className="w-full shrink-0 bg-[#5865F2] text-white hover:bg-[#4752c4] sm:w-auto"
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
          {report.issue_status && ` · ${STATUS_LABELS[report.issue_status] ?? report.issue_status}`}
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
