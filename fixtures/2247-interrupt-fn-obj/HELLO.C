void interrupt my_isr(void) {
  static int counter;
  counter++;
}
int main(void) {
  return 0;
}
