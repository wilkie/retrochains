struct Node { int v; struct Node *next; };
int main(void) {
  static struct Node n2 = {2, 0};
  static struct Node n1 = {1, &n2};
  return n1.v + n1.next->v;
}
