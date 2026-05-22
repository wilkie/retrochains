struct Wrap { int *data; };
int peek(struct Wrap *w) {
  return w->data[1];
}
