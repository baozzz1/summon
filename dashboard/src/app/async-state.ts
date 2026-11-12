/** 不同通道各自只接纳最新请求；整组失效用于提交 ACK 或身份过期。 */
export class RequestScope {
  private epoch = 0;
  private channels = new Map<string, object>();

  begin(channel: string): () => boolean {
    const epoch = this.epoch;
    const ticket = {};
    this.channels.set(channel, ticket);
    return () => this.epoch === epoch && this.channels.get(channel) === ticket;
  }

  invalidate(channel?: string): void {
    if (channel !== undefined) {
      this.channels.delete(channel);
      return;
    }
    this.epoch += 1;
    this.channels.clear();
  }
}

type SubmissionPhase = 'reviewing' | 'submitting' | 'refreshing';
type SubmissionState<S, R> =
  | { phase: 'idle' }
  | { phase: SubmissionPhase; snapshot: S; completion: Promise<R> };

/** 一次保存只拥有一个快照；重复点击等待同一事务，不重放外部写入。 */
export class Submission<S, R> {
  private state: SubmissionState<S, R> = { phase: 'idle' };

  get pending(): Promise<R> | null {
    return this.state.phase === 'idle' ? null : this.state.completion;
  }

  get snapshot(): S | null {
    return this.state.phase === 'idle' ? null : this.state.snapshot;
  }

  run(snapshot: S, phase: SubmissionPhase, execute: (transition: (phase: SubmissionPhase) => void) => Promise<R>): Promise<R> {
    if (this.state.phase !== 'idle') return this.state.completion;
    let resolve!: (result: R) => void;
    let reject!: (error: unknown) => void;
    const completion = new Promise<R>((yes, no) => { resolve = yes; reject = no; });
    this.state = { phase, snapshot, completion };
    const transition = (next: SubmissionPhase) => {
      if (this.state.phase !== 'idle') this.state = { ...this.state, phase: next };
    };
    void execute(transition).then(
      (result) => { this.state = { phase: 'idle' }; resolve(result); },
      (error: unknown) => { this.state = { phase: 'idle' }; reject(error); },
    );
    return completion;
  }
}
