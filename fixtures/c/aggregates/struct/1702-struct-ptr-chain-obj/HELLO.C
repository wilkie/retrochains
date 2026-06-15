struct Node {
  int v;
  struct Node *next;
};
int main(void) {
  struct Node b;
  struct Node a;
  b.v = 20;
  b.next = (struct Node *)0;
  a.v = 10;
  a.next = &b;
  return a.next->v + a.v;
}
