struct Node {
  int v;
  struct Node *next;
};
int main(void) {
  struct Node a, b, c;
  a.v = 1; a.next = &b;
  b.v = 2; b.next = &c;
  c.v = 3; c.next = 0;
  return a.v + a.next->v + a.next->next->v;
}
