struct node {
  int value;
  struct node *next;
};
struct node head;
int main(void) {
  head.value = 7;
  head.next = &head;
  return 0;
}
