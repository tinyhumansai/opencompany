import { MessageCircleHeart } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
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
import { DISCORD_INVITE_URL } from "@/lib/links";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/** A standalone feedback surface, plus a nudge to the community. */
export function FeedbackView({ client, company }: Props) {
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
            </CardDescription>
          </CardHeader>
          <CardContent>
            <FeedbackForm client={client} company={company} onDone={() => {}} showCancel={false} />
          </CardContent>
        </Card>

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
