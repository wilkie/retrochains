struct Node {
  struct Node *next;
  int v;
};

int sum(struct Node *p) {
  int s = 0;
  while (p) {
    s += p->v;
    p = p->next;
  }
  return s;
}
