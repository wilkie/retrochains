struct Node { int v; struct Node *next; };
struct Node head = { 42, 0 };
int main(void) {
  return head.v;
}
