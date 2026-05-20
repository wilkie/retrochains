char *greet(void) {
  return "hi";
}
int main(void) {
  return greet()[0];
}
