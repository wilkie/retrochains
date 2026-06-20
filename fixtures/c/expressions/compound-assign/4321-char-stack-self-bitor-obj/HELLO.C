int sink(void) {
  return 0;
}
int main(void) {
  char c = 12;
  c = c | 8;
  sink();
  return c;
}
