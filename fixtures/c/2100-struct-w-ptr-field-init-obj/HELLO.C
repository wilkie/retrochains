struct Item { char *name; int qty; };
int main(void) {
  static struct Item it = {"apple", 5};
  return it.qty;
}
