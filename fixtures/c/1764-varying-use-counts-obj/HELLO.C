int main(void) {
  int rare = 1;
  int often = 10;
  int seldom = 100;
  rare = rare + 1;
  often = often + 1;
  often = often * 2;
  often = often - 5;
  seldom = seldom + 1;
  return rare + often + seldom;
}
